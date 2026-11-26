//! Sous-commande `camstream healthcheck` de bout en bout : le vrai binaire interroge
//! un vrai serveur HTTP ; code de sortie 0 si sain, 1 si l'instance est en `failed`.

mod support;

use std::net::SocketAddr;
use std::process::{Command, ExitStatus};

use camstream::server::ServerBuilder;
use camstream::stream::{FrameHub, HubCapacity, SourceStatus};
use support::{test_config, within, TestApp};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// Exécute `camstream healthcheck` contre `addr` (hors du runtime : appel bloquant).
async fn run_healthcheck(addr: SocketAddr) -> ExitStatus {
    let task = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_camstream"))
            // Les options explicites priment sur un éventuel `CAMSTREAM_*` hérité.
            .args(["--bind", &addr.ip().to_string(), "--port", &addr.port().to_string()])
            .args(["healthcheck", "--timeout-ms", "2000"])
            .env("RUST_LOG", "error")
            .status()
            .expect("run camstream healthcheck")
    });
    within("healthcheck process", task).await.expect("join")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_healthy_instance_gives_exit_code_0() {
    let app = TestApp::start(test_config(), None).await;
    assert_eq!(run_healthcheck(app.addr).await.code(), Some(0));
    assert_eq!(app.stop().await, camstream::app::Outcome::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_instance_gives_exit_code_1() {
    // Une application complète quitte dès que sa capture est en `failed` : on sert donc
    // le vrai serveur HTTP avec un état `failed` maintenu, comme pendant la fenêtre
    // entre l'échec et la sortie du processus. `/health` répond 503.
    let (status, receiver) = watch::channel(SourceStatus::Failed {
        reason: "permission denied on video device: /dev/video0".into(),
    });
    let shutdown = CancellationToken::new();
    let server = ServerBuilder::new("127.0.0.1:0".parse().unwrap())
        .hub(FrameHub::new(HubCapacity::DEFAULT))
        .status(receiver)
        .shutdown(shutdown.clone())
        .build()
        .unwrap()
        .bind()
        .await
        .unwrap();
    let addr = server.local_addr();
    let serving = tokio::spawn(server.serve());

    assert_eq!(run_healthcheck(addr).await.code(), Some(1));
    // Retour à un état sain : la même sonde réussit.
    status.send_replace(SourceStatus::Idle);
    assert_eq!(run_healthcheck(addr).await.code(), Some(0));

    shutdown.cancel();
    within("server stop", serving).await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_instance_gives_exit_code_1() {
    let addr = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    assert_eq!(run_healthcheck(addr).await.code(), Some(1));
}
