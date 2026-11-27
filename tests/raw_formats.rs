//! Tests de bout en bout des formats bruts (ADR 0008) : mire synthétique Bayer `GRBG`
//! (table de capacités de la caméra de l'utilisateur) et YUYV, négociée, convertie en
//! JPEG dans le thread de capture puis diffusée en multipart, et décrite par `/health`.

mod support;

use camstream::app::Outcome;
use camstream::domain::{FormatPreference, JpegQuality, WhiteBalance};
use camstream::stream::SourceStatus;
use hyper::StatusCode;
use support::{body_text, get, test_config, wait_status, MultipartReader, TestApp};

/// Décode un JPEG reçu et renvoie (largeur, hauteur, pixels RGB).
fn decode(jpeg: &[u8]) -> (u16, u16, Vec<u8>) {
    let mut decoder = jpeg_decoder::Decoder::new(jpeg);
    let pixels = decoder
        .decode()
        .expect("the browser would decode this JPEG");
    let info = decoder.info().expect("JPEG header");
    (info.width, info.height, pixels)
}

/// Lit `/health` et renvoie le JSON.
async fn health_json(addr: std::net::SocketAddr) -> serde_json::Value {
    let response = get(addr, "/health").await;
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_str(&body_text(response).await).expect("JSON body")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_grbg_camera_is_streamed_as_jpeg_at_324x240() {
    let mut config = test_config();
    // Demande par défaut (640x480) : la négociation doit retenir 324x240.
    config.capture.format = FormatPreference::Bayer;
    config.capture.encoding.quality = JpegQuality::new(70).unwrap();
    let mut app = TestApp::start(config, None).await;

    let response = get(app.addr, "/stream.mjpg").await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut reader = MultipartReader::new(response);
    let first = reader.next_part().await.expect("first part");
    let second = reader.next_part().await.expect("second part");
    for part in [&first, &second] {
        assert_eq!(part.header("content-type"), Some("image/jpeg"));
        assert!(part.data.starts_with(&[0xFF, 0xD8]), "SOI");
        assert!(part.data.ends_with(&[0xFF, 0xD9]), "EOI");
        let (width, height, pixels) = decode(&part.data);
        assert_eq!((width, height), (324, 240));
        assert_eq!(pixels.len(), 324 * 240 * 3);
    }
    // Le carré de la mire bouge : les trames diffèrent.
    assert_ne!(first.data, second.data);

    wait_status(&mut app.status, "Streaming", |status| {
        matches!(status, SourceStatus::Streaming { .. })
    })
    .await;
    let json = health_json(app.addr).await;
    assert_eq!(json["camera"]["state"], "streaming");
    assert_eq!(
        json["camera"]["source"],
        serde_json::json!({
            "name": "synthetic test pattern",
            "pixel_format": "GRBG",
            "resolution": "324x240",
            "fps": 50,
            "fps_origin": "confirmed",
            "encoding": "software",
            "jpeg_quality": 70
        })
    );
    drop(reader);
    assert_eq!(app.stop().await, Outcome::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_yuyv_source_is_streamed_without_white_balance() {
    let mut config = test_config();
    config.capture.format = FormatPreference::Yuyv;
    config.capture.encoding.white_balance = WhiteBalance::Off;
    let mut app = TestApp::start(config, None).await;
    let response = get(app.addr, "/stream.mjpg").await;
    let mut reader = MultipartReader::new(response);
    let part = reader.next_part().await.expect("first part");
    let (width, height, _) = decode(&part.data);
    assert_eq!((width, height), (640, 480));
    wait_status(&mut app.status, "Streaming", |status| {
        matches!(status, SourceStatus::Streaming { .. })
    })
    .await;
    let json = health_json(app.addr).await;
    assert_eq!(json["camera"]["source"]["pixel_format"], "YUYV");
    // 50 i/s demandés, 30 proposés au plus par la table YUYV simulée.
    assert_eq!(json["camera"]["source"]["fps"], 30);
    drop(reader);
    assert_eq!(app.stop().await, Outcome::Stopped);
}
