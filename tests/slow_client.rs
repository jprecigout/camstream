//! Client lent de bout en bout (ADR 0004) : un navigateur qui ne lit plus le flux
//! saute des trames (`Lagged` absorbé par `FrameSubscriber`) au lieu de bloquer la
//! capture, puis reprend sur la même connexion avec les trames récentes.
//!
//! Sans attente arbitraire : le test attend que la source ait produit plus de trames
//! que tout ce que la chaîne peut retenir (tampon du hub, tampons de hyper et du noyau),
//! ce qui rend la perte de trames certaine et non dépendante du temps.

mod support;

use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Empty;
use hyper::{header, StatusCode};
use hyper_util::rt::TokioIo;
use support::{
    eventually, frame_sequence, test_config, wait_status, within, MultipartReader, ScriptedFactory,
    TestApp,
};
use tokio::net::TcpSocket;

/// Taille de chaque trame : 1 Mio, pour saturer vite les tampons.
const FRAME_SIZE: usize = 1 << 20;

/// Ouvre `/stream.mjpg` avec un tampon de réception minimal, sans lire le corps.
async fn open_slow_stream(addr: SocketAddr) -> hyper::Response<hyper::body::Incoming> {
    within("slow client response", async {
        let socket = TcpSocket::new_v4().expect("socket");
        socket.set_recv_buffer_size(4096).expect("SO_RCVBUF");
        let stream = socket.connect(addr).await.expect("connect");
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .expect("handshake");
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let request = hyper::Request::get("/stream.mjpg")
            .header(header::HOST, addr.to_string())
            .body(Empty::<Bytes>::new())
            .expect("request");
        sender.send_request(request).await.expect("response")
    })
    .await
}

/// Nombre de trames que la chaîne ne peut en aucun cas retenir en entier : tampon
/// d'envoi maximal du noyau (`tcp_wmem`) plus une large marge (hub, hyper, tampon de
/// réception du client).
fn frames_beyond_any_buffering() -> usize {
    let wmem_max = std::fs::read_to_string("/proc/sys/net/ipv4/tcp_wmem")
        .ok()
        .and_then(|text| text.split_whitespace().nth(2)?.parse::<usize>().ok())
        .unwrap_or(4 << 20);
    (wmem_max / FRAME_SIZE + 64).max(100)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_client_skips_frames_without_blocking_the_capture() {
    let (factory, probe) =
        ScriptedFactory::with_frame_size([], Duration::from_millis(2), FRAME_SIZE);
    let mut app = TestApp::start(test_config(), Some(factory)).await;
    wait_status(&mut app.status, "Idle", |s| {
        *s == camstream::stream::SourceStatus::Idle
    })
    .await;

    let response = open_slow_stream(app.addr).await;
    assert_eq!(response.status(), StatusCode::OK);
    // Le client ne lit rien : la capture continue malgré tout (aucun blocage).
    let target = frames_beyond_any_buffering();
    eventually("capture running past the stalled client", || {
        probe.frames() >= target
    })
    .await;

    // Le client se remet à lire : il reçoit d'abord ce qui était en tampon, puis la
    // suite après un saut de numéros (trames perdues pour lui seul).
    let mut reader = MultipartReader::new(response);
    let mut sequences = Vec::new();
    while sequences.len() < 40 {
        let part = reader.next_part().await.expect("part");
        assert_eq!(part.data.len(), FRAME_SIZE);
        sequences.push(frame_sequence(&part.data).expect("numbered frame"));
    }
    assert!(
        sequences.windows(2).all(|pair| pair[1] > pair[0]),
        "order preserved: {sequences:?}"
    );
    assert!(
        sequences.windows(2).any(|pair| pair[1] > pair[0] + 1),
        "frames skipped: {sequences:?}"
    );
    drop(reader);
    assert_eq!(app.stop().await, camstream::app::Outcome::Stopped);
}
