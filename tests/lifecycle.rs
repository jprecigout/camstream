//! Tests du cycle de vie de l'application (`App` / `BoundApp::run`) : arrêt demandé,
//! erreur fatale de capture, panique du thread de capture → code de sortie 1.

mod support;

use std::time::Duration;

use camstream::app::{App, Outcome};
use camstream::domain::SourceKind;
use camstream::stream::SourceStatus;
use support::{get, test_config, wait_status, within, Failure, ScriptedFactory, Step, TestApp};

const INTERVAL: Duration = Duration::from_millis(5);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requested_shutdown_stops_everything_cleanly() {
    let (factory, probe) = ScriptedFactory::new([], INTERVAL);
    let mut app = TestApp::start(test_config(), Some(factory)).await;
    wait_status(&mut app.status, "Idle", |s| *s == SourceStatus::Idle).await;
    let mut status = app.status.clone();
    assert_eq!(app.stop().await, Outcome::Stopped);
    assert_eq!(*status.borrow_and_update(), SourceStatus::Stopped);
    assert_eq!(probe.live(), 0, "camera released");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fatal_capture_error_stops_the_application_with_failure() {
    let (factory, _probe) = ScriptedFactory::new([Step::Fail(Failure::PermissionDenied)], INTERVAL);
    let app = TestApp::start(test_config(), Some(factory)).await;
    // Aucun signal : l'application s'arrête d'elle-même.
    assert_eq!(app.outcome().await, Outcome::CaptureFailed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_capture_thread_panic_stops_the_application_with_failure() {
    let (factory, _probe) = ScriptedFactory::new(
        [
            Step::HEALTHY,
            Step::Source {
                frames: Some(0),
                then: Failure::Panic,
            },
        ],
        INTERVAL,
    );
    let mut app = TestApp::start(test_config(), Some(factory)).await;
    wait_status(&mut app.status, "Idle", |s| *s == SourceStatus::Idle).await;
    // Le premier spectateur déclenche l'ouverture, puis la panique de la source.
    let addr = app.addr;
    let viewer = tokio::spawn(async move { get(addr, "/stream.mjpg").await });
    assert_eq!(app.outcome().await, Outcome::CaptureFailed);
    viewer.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_real_factory_reports_a_missing_webcam_as_idle() {
    // Webcam absente au démarrage : avertissement et état `Idle`, pas d'échec
    // (sans la feature `v4l2`, la source V4L2 est indisponible : échec fatal).
    let mut config = test_config();
    config.capture.kind = SourceKind::V4l2;
    config.capture.device = camstream::domain::DevicePath::new("/dev/camstream-test-missing");
    let mut app = TestApp::start(config, None).await;
    if cfg!(feature = "v4l2") {
        wait_status(&mut app.status, "Idle", |s| *s == SourceStatus::Idle).await;
        assert_eq!(app.stop().await, Outcome::Stopped);
    } else {
        assert_eq!(app.outcome().await, Outcome::CaptureFailed);
    }
}

#[tokio::test]
async fn a_current_thread_runtime_is_refused() {
    // `Handle::block_on` ne ferait pas avancer les minuteries de ce runtime.
    let error = App::new(test_config()).bind().await.err().expect("refused");
    assert!(error.to_string().contains("multi-thread"), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_is_bounded_even_with_a_connected_viewer() {
    let mut config = test_config();
    config.shutdown_timeout = Duration::from_secs(2);
    let app = TestApp::start(config, None).await;
    let _viewer = get(app.addr, "/stream.mjpg").await;
    let started = tokio::time::Instant::now();
    assert_eq!(within("stop", app.stop()).await, Outcome::Stopped);
    assert!(started.elapsed() < Duration::from_secs(3));
}
