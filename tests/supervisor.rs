//! Tests d'intégration du `CaptureSupervisor` : sonde de démarrage, capture à la
//! demande, délai de grâce, reconnexion, erreurs fatales, panique et arrêt.
//!
//! Runtime **multi-thread** obligatoire : le thread de capture attend via
//! `Handle::block_on`, qui ne fait pas avancer les minuteries d'un runtime
//! `current_thread`. Le thread de test n'est jamais bloqué (`join` via
//! `spawn_blocking`).

mod support;

use std::time::Duration;

use camstream::stream::{
    BackoffPolicy, CaptureHandle, CaptureSupervisor, CaptureThreadError, FrameHub, HubCapacity,
    SourceStatus, StatusReceiver, SupervisorSettings,
};
use support::{eventually, wait_status, within, Failure, Probe, ScriptedFactory, Step};
use tokio_util::sync::CancellationToken;

/// Intervalle entre deux trames des sources scriptées.
const FRAME_INTERVAL: Duration = Duration::from_millis(5);

struct Harness {
    hub: FrameHub,
    status: StatusReceiver,
    probe: Probe,
    cancel: CancellationToken,
    handle: Option<CaptureHandle>,
}

impl Harness {
    fn start(steps: impl IntoIterator<Item = Step>, settings: SupervisorSettings) -> Self {
        Self::start_with(steps, settings, |_| ())
    }

    /// Démarre le superviseur ; `before_spawn` agit sur le hub avant le lancement du
    /// thread (abonnés déjà présents à la sonde).
    fn start_with(
        steps: impl IntoIterator<Item = Step>,
        settings: SupervisorSettings,
        before_spawn: impl FnOnce(&FrameHub),
    ) -> Self {
        let (factory, probe) = ScriptedFactory::new(steps, FRAME_INTERVAL);
        // Tampon large (64 trames, soit 320 ms à 5 ms par trame) : un test ralenti par
        // une machine chargée ne doit pas sauter de trames (`Lagged`), sinon les
        // assertions sur les numéros de séquence deviendraient aléatoires. Les sauts
        // de trames sont testés à part (`stream::hub`, `tests/slow_client.rs`).
        let hub = FrameHub::new(HubCapacity::new(64).expect("non-zero capacity"));
        let cancel = CancellationToken::new();
        let supervisor = CaptureSupervisor::new(
            factory,
            hub.clone(),
            settings,
            tokio::runtime::Handle::current(),
            cancel.child_token(),
        );
        let status = supervisor.status();
        before_spawn(&hub);
        let handle = supervisor.spawn().expect("spawn capture thread");
        Self {
            hub,
            status,
            probe,
            cancel,
            handle: Some(handle),
        }
    }

    async fn wait(
        &mut self,
        what: &str,
        predicate: impl FnMut(&SourceStatus) -> bool,
    ) -> SourceStatus {
        wait_status(&mut self.status, what, predicate).await
    }

    /// Attend la fin du thread sans bloquer le runtime.
    async fn join(&mut self) -> Result<(), CaptureThreadError> {
        let handle = self.handle.take().expect("not joined yet");
        within(
            "capture thread end",
            tokio::task::spawn_blocking(move || handle.join()),
        )
        .await
        .expect("join task")
    }

    async fn stop(mut self) {
        self.cancel.cancel();
        self.wait("Stopped", |s| *s == SourceStatus::Stopped).await;
        self.join().await.expect("clean stop");
        assert_eq!(self.probe.live(), 0, "camera released after stop");
    }
}

fn settings(idle_timeout: Duration, backoff_initial: Duration) -> SupervisorSettings {
    SupervisorSettings {
        backoff: BackoffPolicy::new(backoff_initial, backoff_initial * 4),
        idle_timeout,
        ..SupervisorSettings::default()
    }
}

fn is_streaming(status: &SourceStatus) -> bool {
    matches!(status, SourceStatus::Streaming { .. })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_probe_opens_then_closes_the_camera() {
    let mut harness = Harness::start(
        [],
        settings(Duration::from_millis(50), Duration::from_millis(20)),
    );
    harness.wait("Idle", |s| *s == SourceStatus::Idle).await;
    assert_eq!(harness.probe.creates(), 1, "one probe");
    assert_eq!(harness.probe.live(), 0, "camera closed after the probe");
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capture_starts_with_the_first_subscriber_and_stops_after_the_grace_period() {
    let mut harness = Harness::start(
        [],
        settings(Duration::from_millis(50), Duration::from_millis(20)),
    );
    harness.wait("Idle", |s| *s == SourceStatus::Idle).await;

    let mut subscriber = harness.hub.subscribe();
    within("first frame", subscriber.next_frame())
        .await
        .expect("frame");
    harness.wait("Streaming", is_streaming).await;
    assert_eq!(harness.probe.creates(), 2, "probe + first viewer");
    assert_eq!(harness.probe.live(), 1);

    drop(subscriber);
    harness
        .wait("Idle after the grace period", |s| *s == SourceStatus::Idle)
        .await;
    eventually("camera released", || harness.probe.live() == 0).await;
    assert_eq!(harness.probe.creates(), 2);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quick_resubscription_keeps_the_camera_on() {
    // Délai de grâce long : le rechargement de page le retrouve ouvert.
    let mut harness = Harness::start(
        [],
        settings(Duration::from_secs(5), Duration::from_millis(20)),
    );
    harness.wait("Idle", |s| *s == SourceStatus::Idle).await;

    let mut subscriber = harness.hub.subscribe();
    within("first frame", subscriber.next_frame())
        .await
        .expect("frame");
    drop(subscriber);
    tokio::time::sleep(Duration::from_millis(30)).await;
    let mut subscriber = harness.hub.subscribe();
    within("frame after reload", subscriber.next_frame())
        .await
        .expect("frame");

    assert_eq!(harness.probe.creates(), 2, "camera not reopened");
    assert!(is_streaming(&harness.status.borrow()));
    drop(subscriber);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_idle_timeout_turns_the_camera_off_right_away() {
    let mut harness = Harness::start([], settings(Duration::ZERO, Duration::from_millis(20)));
    harness.wait("Idle", |s| *s == SourceStatus::Idle).await;
    let mut subscriber = harness.hub.subscribe();
    within("first frame", subscriber.next_frame())
        .await
        .expect("frame");
    drop(subscriber);
    harness.wait("Idle", |s| *s == SourceStatus::Idle).await;
    eventually("camera released", || harness.probe.live() == 0).await;
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribers_present_at_startup_keep_the_probed_source() {
    let mut subscriber = None;
    let mut harness = Harness::start_with(
        [],
        settings(Duration::from_millis(50), Duration::from_millis(20)),
        |hub| subscriber = Some(hub.subscribe()),
    );
    let mut subscriber = subscriber.expect("subscribed");
    within("first frame", subscriber.next_frame())
        .await
        .expect("frame");
    harness.wait("Streaming", is_streaming).await;
    assert_eq!(harness.probe.creates(), 1, "the probe source is reused");
    drop(subscriber);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_openings_are_retried_with_backoff_while_viewers_wait() {
    let backoff = Duration::from_millis(100);
    let mut harness = Harness::start(
        [
            Step::HEALTHY, // sonde
            Step::Fail(Failure::Disconnected),
            Step::Fail(Failure::Timeout),
        ],
        settings(Duration::from_millis(50), backoff),
    );
    harness.wait("Idle", |s| *s == SourceStatus::Idle).await;

    let started = tokio::time::Instant::now();
    let mut subscriber = harness.hub.subscribe();
    harness
        .wait("Reconnecting", |s| {
            matches!(s, SourceStatus::Reconnecting { .. })
        })
        .await;
    within("frame after reconnection", subscriber.next_frame())
        .await
        .expect("frame");
    harness.wait("Streaming", is_streaming).await;

    // Sonde + 2 échecs + succès ; attentes de 100 ms puis 200 ms entre les tentatives.
    assert_eq!(harness.probe.creates(), 4);
    assert!(
        started.elapsed() >= backoff * 3,
        "backoff respected: {:?}",
        started.elapsed()
    );
    drop(subscriber);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_camera_is_reconnected_on_the_same_subscription() {
    let mut harness = Harness::start(
        [
            Step::HEALTHY, // sonde
            Step::Source {
                frames: Some(3),
                then: Failure::Disconnected,
            },
        ],
        settings(Duration::from_secs(5), Duration::from_millis(20)),
    );
    harness.wait("Idle", |s| *s == SourceStatus::Idle).await;
    let mut subscriber = harness.hub.subscribe();
    let mut sequences = Vec::new();
    for _ in 0..6 {
        let frame = within("frame", subscriber.next_frame())
            .await
            .expect("frame");
        sequences.push(frame.sequence().get());
    }
    // La nouvelle source renumérote ses trames depuis 0 : la reprise est visible.
    assert_eq!(&sequences[..4], &[0, 1, 2, 0], "{sequences:?}");
    assert_eq!(harness.probe.creates(), 3);
    assert_eq!(
        harness.probe.live(),
        1,
        "old source released before the new one"
    );
    drop(subscriber);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnection_is_abandoned_when_the_last_viewer_leaves() {
    let mut harness = Harness::start(
        [
            Step::HEALTHY, // sonde
            Step::Fail(Failure::Disconnected),
            Step::Fail(Failure::Disconnected),
            Step::Fail(Failure::Disconnected),
            Step::Fail(Failure::Disconnected),
        ],
        settings(Duration::from_millis(50), Duration::from_millis(500)),
    );
    harness.wait("Idle", |s| *s == SourceStatus::Idle).await;
    let subscriber = harness.hub.subscribe();
    harness
        .wait("Reconnecting", |s| {
            matches!(s, SourceStatus::Reconnecting { .. })
        })
        .await;
    drop(subscriber);
    harness.wait("Idle", |s| *s == SourceStatus::Idle).await;
    let creates = harness.probe.creates();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(harness.probe.creates(), creates, "no retry without viewers");

    // Au retour d'un spectateur, la première tentative est immédiate (compteur remis à 0).
    let _subscriber = harness.hub.subscribe();
    harness
        .wait("Reconnecting attempt 1", |s| {
            matches!(s, SourceStatus::Reconnecting { attempt: 1, .. })
        })
        .await;
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_camera_at_startup_is_retried_only_with_viewers() {
    let mut harness = Harness::start(
        [Step::Fail(Failure::Disconnected)],
        settings(Duration::from_millis(50), Duration::from_millis(20)),
    );
    harness.wait("Idle", |s| *s == SourceStatus::Idle).await;
    assert_eq!(harness.probe.creates(), 1);
    let mut subscriber = harness.hub.subscribe();
    within("frame", subscriber.next_frame())
        .await
        .expect("frame");
    assert_eq!(
        harness.probe.creates(),
        2,
        "immediate attempt on the first viewer"
    );
    drop(subscriber);
    harness.stop().await;
}

fn corruption_settings(max_corrupted_frames: u32, backoff: Duration) -> SupervisorSettings {
    SupervisorSettings {
        max_corrupted_frames: max_corrupted_frames.try_into().unwrap(),
        ..settings(Duration::from_secs(5), backoff)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn isolated_corrupted_frames_are_skipped_and_the_counter_resets() {
    // 4 trames corrompues puis 1 valide, en boucle, avec un seuil de 5 : le compteur
    // repart de zéro à chaque trame valide, la source n'est jamais rouverte.
    let mut harness = Harness::start(
        [Step::HEALTHY, Step::Intermittent { corrupted_run: 4 }],
        corruption_settings(5, Duration::from_millis(20)),
    );
    harness.wait("Idle", |s| *s == SourceStatus::Idle).await;
    let mut subscriber = harness.hub.subscribe();
    for _ in 0..6 {
        within("valid frame", subscriber.next_frame())
            .await
            .expect("frame");
    }
    assert_eq!(harness.probe.creates(), 2, "never reopened");
    assert!(is_streaming(&harness.status.borrow()));
    drop(subscriber);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_of_corrupted_frames_is_reopened() {
    let mut harness = Harness::start(
        [
            Step::HEALTHY, // sonde
            Step::Source {
                frames: Some(2),
                then: Failure::Corrupted,
            },
        ],
        // Attente de 300 ms : laisse le temps d'observer `Reconnecting`.
        corruption_settings(5, Duration::from_millis(300)),
    );
    harness.wait("Idle", |s| *s == SourceStatus::Idle).await;
    let mut subscriber = harness.hub.subscribe();
    for _ in 0..2 {
        within("frame", subscriber.next_frame())
            .await
            .expect("frame");
    }
    let status = harness
        .wait("Reconnecting", |s| {
            matches!(s, SourceStatus::Reconnecting { .. })
        })
        .await;
    assert_eq!(
        status,
        SourceStatus::Reconnecting {
            attempt: 1,
            last_error: "5 consecutive corrupted frames".into()
        }
    );
    // Nouvelle source saine : la même souscription reçoit de nouveau des trames.
    let frame = within("frame after reopening", subscriber.next_frame())
        .await
        .expect("frame");
    // Trame de la nouvelle source : son numéro repart de 0, décalé seulement des
    // trames sautées si le test a été ralenti (aucune avant : 2 trames publiées, 2 lues).
    assert_eq!(
        frame.sequence().get(),
        subscriber.skipped_frames(),
        "frame of the new source"
    );
    assert_eq!(harness.probe.creates(), 3);
    assert_eq!(harness.probe.live(), 1, "corrupted source released");
    drop(subscriber);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fatal_probe_error_fails_the_capture() {
    let mut harness = Harness::start(
        [Step::Fail(Failure::PermissionDenied)],
        settings(Duration::from_millis(50), Duration::from_millis(20)),
    );
    let status = harness.wait("Failed", SourceStatus::is_failed).await;
    assert_eq!(
        status,
        SourceStatus::Failed {
            reason: "permission denied on video device: /dev/video0".into()
        }
    );
    let error = harness.join().await.unwrap_err();
    assert!(matches!(error, CaptureThreadError::Fatal(_)), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fatal_error_while_reconnecting_fails_the_capture() {
    let mut harness = Harness::start(
        [
            Step::HEALTHY,
            Step::Fail(Failure::Disconnected),
            Step::Fail(Failure::PermissionDenied),
        ],
        settings(Duration::from_millis(50), Duration::from_millis(20)),
    );
    harness.wait("Idle", |s| *s == SourceStatus::Idle).await;
    let _subscriber = harness.hub.subscribe();
    harness.wait("Failed", SourceStatus::is_failed).await;
    assert!(matches!(
        harness.join().await,
        Err(CaptureThreadError::Fatal(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panic_in_the_capture_thread_is_reported_as_failed() {
    let mut harness = Harness::start(
        [
            Step::HEALTHY,
            Step::Source {
                frames: Some(1),
                then: Failure::Panic,
            },
        ],
        settings(Duration::from_millis(50), Duration::from_millis(20)),
    );
    harness.wait("Idle", |s| *s == SourceStatus::Idle).await;
    let _subscriber = harness.hub.subscribe();
    let status = harness.wait("Failed", SourceStatus::is_failed).await;
    assert_eq!(
        status,
        SourceStatus::Failed {
            reason: "capture thread panicked".into()
        }
    );
    match harness.join().await {
        Err(CaptureThreadError::Panicked(message)) => {
            assert_eq!(message, "simulated capture bug");
        }
        other => panic!("expected a panic, got {other:?}"),
    }
    assert_eq!(harness.probe.live(), 0, "source dropped during unwinding");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_stops_a_streaming_capture_and_releases_the_camera() {
    let mut harness = Harness::start(
        [],
        settings(Duration::from_secs(5), Duration::from_millis(20)),
    );
    harness.wait("Idle", |s| *s == SourceStatus::Idle).await;
    let mut subscriber = harness.hub.subscribe();
    within("frame", subscriber.next_frame())
        .await
        .expect("frame");
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_interrupts_a_long_backoff() {
    let mut harness = Harness::start(
        [Step::HEALTHY, Step::Fail(Failure::Disconnected)],
        settings(Duration::from_millis(50), Duration::from_secs(60)),
    );
    harness.wait("Idle", |s| *s == SourceStatus::Idle).await;
    let _subscriber = harness.hub.subscribe();
    harness
        .wait("Reconnecting", |s| {
            matches!(s, SourceStatus::Reconnecting { .. })
        })
        .await;
    let started = tokio::time::Instant::now();
    harness.stop().await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "60 s backoff interrupted"
    );
}
