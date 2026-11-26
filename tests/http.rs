//! Tests d'intégration HTTP de bout en bout : application complète sur un port
//! libre avec la mire synthétique, client HTTP/1.1 réel, flux multipart lu morceau
//! par morceau.

mod support;

use std::time::Duration;

use axum::body::Body;
use camstream::server::{health, MaxClients, ServerBuilder, ServerError};
use camstream::stream::{FrameHub, HubCapacity, SourceStatus};
use http_body_util::BodyExt;
use hyper::{header, Method, StatusCode};
use support::{
    body_text, get, request, test_config, wait_status, within, MultipartReader, TestApp,
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

/// Lit `/health` et renvoie le code et le JSON.
async fn health_json(addr: std::net::SocketAddr) -> (StatusCode, serde_json::Value) {
    let response = get(addr, "/health").await;
    let status = response.status();
    let body = body_text(response).await;
    (status, serde_json::from_str(&body).expect("JSON body"))
}

/// Ouvre `/stream.mjpg` et attend la première partie (place réservée, abonnement actif).
async fn open_stream(addr: std::net::SocketAddr) -> MultipartReader {
    let response = get(addr, "/stream.mjpg").await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut reader = MultipartReader::new(response);
    reader.next_part().await.expect("first part");
    reader
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn index_page_shows_the_stream_and_a_waiting_message() {
    let app = TestApp::start(test_config(), None).await;
    let response = get(app.addr, "/").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers()[header::CONTENT_TYPE]
        .to_str()
        .unwrap()
        .starts_with("text/html"));
    let page = body_text(response).await;
    assert!(page.contains(r#"src="/stream.mjpg""#));
    assert!(page.contains("Démarrage de la caméra…"));
    assert_eq!(app.stop().await, camstream::app::Outcome::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_delivers_complete_jpeg_parts() {
    let app = TestApp::start(test_config(), None).await;
    let response = get(app.addr, "/stream.mjpg").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "multipart/x-mixed-replace; boundary=frame"
    );
    assert!(response.headers()[header::CACHE_CONTROL]
        .to_str()
        .unwrap()
        .contains("no-store"));

    let mut reader = MultipartReader::new(response);
    let mut parts = Vec::new();
    for _ in 0..5 {
        parts.push(reader.next_part().await.expect("part"));
    }
    for part in &parts {
        assert_eq!(part.header("content-type"), Some("image/jpeg"));
        let length: usize = part.header("content-length").unwrap().parse().unwrap();
        assert_eq!(part.data.len(), length);
        assert!(part.data.starts_with(&[0xFF, 0xD8]), "SOI");
        assert!(part.data.ends_with(&[0xFF, 0xD9]), "EOI");
    }
    // La mire bouge : deux parties consécutives diffèrent.
    assert_ne!(parts[0].data, parts[1].data);
    // Corps reçu en plusieurs morceaux (en-tête puis image, pour chaque partie).
    assert!(reader.chunks >= 5, "chunks: {}", reader.chunks);
    drop(reader);
    assert_eq!(app.stop().await, camstream::app::Outcome::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clients_beyond_the_limit_get_503_until_a_place_is_freed() {
    let mut config = test_config();
    config.max_clients = 2.try_into().unwrap();
    let app = TestApp::start(config, None).await;

    let first = open_stream(app.addr).await;
    let _second = open_stream(app.addr).await;
    let refused = get(app.addr, "/stream.mjpg").await;
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(refused.headers()[header::RETRY_AFTER], "5");

    // Le client part : sa place est rendue dès que le serveur constate la coupure
    // (écriture de la trame suivante en échec).
    drop(first);
    within("a freed place", async {
        while health_json(app.addr).await.1["clients"] != 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    let _third = open_stream(app.addr).await;
    assert_eq!(app.stop().await, camstream::app::Outcome::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_reports_camera_state_and_clients() {
    let mut app = TestApp::start(test_config(), None).await;
    wait_status(&mut app.status, "Idle", |s| *s == SourceStatus::Idle).await;
    let (code, json) = health_json(app.addr).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        json,
        serde_json::json!({"status": "ok", "camera": {"state": "idle"}, "clients": 0})
    );

    let stream = open_stream(app.addr).await;
    let (code, json) = health_json(app.addr).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(json["camera"]["state"], "streaming");
    assert_eq!(json["camera"]["source"]["name"], "synthetic test pattern");
    assert_eq!(json["camera"]["source"]["resolution"], "640x480");
    assert_eq!(json["camera"]["source"]["fps"], 50);
    assert_eq!(json["clients"], 1);

    // Dernier client parti : la webcam s'éteint après le délai de grâce (200 ms).
    drop(stream);
    wait_status(&mut app.status, "Idle again", |s| *s == SourceStatus::Idle).await;
    let (_, json) = health_json(app.addr).await;
    assert_eq!(json["clients"], 0);
    assert_eq!(app.stop().await, camstream::app::Outcome::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthcheck_client_accepts_a_running_instance() {
    let app = TestApp::start(test_config(), None).await;
    let target = health::healthcheck_target(app.addr.ip(), app.addr.port());
    tokio::task::spawn_blocking(move || health::check(target, Duration::from_secs(2)))
        .await
        .unwrap()
        .expect("healthy");
    assert_eq!(app.stop().await, camstream::app::Outcome::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_routes_and_methods_are_rejected() {
    let app = TestApp::start(test_config(), None).await;
    assert_eq!(get(app.addr, "/nope").await.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        request(app.addr, Method::POST, "/health").await.status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(app.stop().await, camstream::app::Outcome::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_ends_open_streams() {
    let app = TestApp::start(test_config(), None).await;
    let mut stream = open_stream(app.addr).await;
    let outcome = app.stop().await;
    assert_eq!(outcome, camstream::app::Outcome::Stopped);
    // Le flux se termine proprement au lieu de bloquer l'arrêt gracieux.
    while stream.next_part().await.is_some() {}
}

// --- Tests au niveau du serveur seul (sans capture), via `Router::oneshot`. ---

fn server_with_status(
    status: SourceStatus,
) -> (camstream::server::Server, watch::Sender<SourceStatus>) {
    let (sender, receiver) = watch::channel(status);
    let server = ServerBuilder::new("127.0.0.1:0".parse().unwrap())
        .hub(FrameHub::new(HubCapacity::DEFAULT))
        .status(receiver)
        .max_clients(MaxClients::new(1).unwrap())
        .shutdown(CancellationToken::new())
        .build()
        .unwrap();
    (server, sender)
}

async fn oneshot_health(server: &camstream::server::Server) -> (StatusCode, serde_json::Value) {
    let response = server
        .router()
        .oneshot(hyper::Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let code = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (code, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn health_is_503_when_the_capture_failed() {
    let (server, sender) = server_with_status(SourceStatus::Reconnecting {
        attempt: 2,
        last_error: "video device disconnected".into(),
    });
    let (code, json) = oneshot_health(&server).await;
    assert_eq!(code, StatusCode::OK, "reconnecting is healthy");
    assert_eq!(json["camera"]["attempt"], 2);

    sender.send_replace(SourceStatus::Failed {
        reason: "permission denied on video device: /dev/video0".into(),
    });
    let (code, json) = oneshot_health(&server).await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json["status"], "failed");
    assert_eq!(
        json["camera"]["error"],
        "permission denied on video device: /dev/video0"
    );
}

#[test]
fn the_builder_requires_its_mandatory_parameters() {
    let error = ServerBuilder::new("127.0.0.1:0".parse().unwrap())
        .hub(FrameHub::new(HubCapacity::DEFAULT))
        .shutdown(CancellationToken::new())
        .build()
        .unwrap_err();
    assert!(matches!(error, ServerError::MissingParameter("status")));
}

#[tokio::test]
async fn binding_an_occupied_port_fails() {
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let (sender, receiver) = watch::channel(SourceStatus::Idle);
    let error = ServerBuilder::new(occupied.local_addr().unwrap())
        .hub(FrameHub::new(HubCapacity::DEFAULT))
        .status(receiver)
        .shutdown(CancellationToken::new())
        .build()
        .unwrap()
        .run()
        .await
        .unwrap_err();
    assert!(matches!(error, ServerError::Bind { .. }), "{error}");
    drop(sender);
}
