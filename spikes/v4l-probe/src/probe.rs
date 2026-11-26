//! Scénarios de mesure, indépendants du matériel.
//!
//! Chaque scénario ne dépend que des traits `FrameStream` / `StreamOpener` : la
//! même logique est exécutée sur la webcam (adaptateur `device`) et dans les tests
//! (flux scriptés).

use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::error::ErrnoKind;
use crate::jpeg;
use crate::stats::{CaptureStats, CaptureSummary};
use crate::stream::{FrameStream, OpenTimings, StreamOpener};

/// Convertit une durée en millisecondes décimales (format des rapports).
pub fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// Latence entre le début de l'ouverture et la première trame (ADR 0006).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct StartupLatency {
    /// Ouverture du nœud.
    pub open_ms: f64,
    /// Négociation format et cadence.
    pub configure_ms: f64,
    /// Allocation des tampons mmap.
    pub buffers_ms: f64,
    /// Du flux prêt à la première trame (inclut `STREAMON`).
    pub first_frame_ms: f64,
    /// Total, du début de l'ouverture à la première trame.
    pub total_ms: f64,
}

impl StartupLatency {
    fn new(timings: OpenTimings, first_frame: Duration) -> Self {
        let total = timings.open + timings.configure + timings.buffers + first_frame;
        Self {
            open_ms: millis(timings.open),
            configure_ms: millis(timings.configure),
            buffers_ms: millis(timings.buffers),
            first_frame_ms: millis(first_frame),
            total_ms: millis(total),
        }
    }
}

/// Échec de démarrage d'un flux.
#[derive(Debug, thiserror::Error)]
pub enum StartupError<E: std::error::Error + 'static> {
    /// L'ouverture ou la configuration a échoué.
    #[error("cannot open stream")]
    Open(#[source] E),
    /// Le flux est ouvert mais aucune première trame n'a été reçue.
    #[error("no first frame ({kind}){}", drop_suffix(.drop_panic))]
    FirstFrame {
        /// Classification de l'erreur.
        kind: ErrnoKind,
        /// Erreur d'origine.
        #[source]
        source: io::Error,
        /// Panique éventuelle du `Drop` du flux détruit après l'échec.
        drop_panic: Option<String>,
    },
}

/// Suffixe du message d'erreur quand la destruction du flux a aussi paniqué.
fn drop_suffix(drop_panic: &Option<String>) -> String {
    drop_panic
        .as_deref()
        .map_or_else(String::new, |message| format!(", then {message}"))
}

/// Ouvre un flux et attend sa première trame, en mesurant chaque étape.
///
/// La première trame est jetée : seule la latence compte ici. En cas d'échec, le flux
/// est détruit via [`timed_drop`] pour qu'une panique du `Drop` de `v4l` soit
/// rapportée au lieu d'interrompre l'outil.
pub fn open_until_first_frame<O: StreamOpener>(
    opener: &mut O,
    timeout: Duration,
) -> Result<(O::Stream, StartupLatency), StartupError<O::Error>> {
    let (mut stream, timings) = opener.open().map_err(StartupError::Open)?;
    stream.set_timeout(timeout);
    let started = Instant::now();
    match stream.next_frame() {
        Ok(_) => Ok((stream, StartupLatency::new(timings, started.elapsed()))),
        Err(source) => {
            let kind = ErrnoKind::classify(&source);
            let (_, drop_panic) = timed_drop(stream);
            Err(StartupError::FirstFrame {
                kind,
                source,
                drop_panic,
            })
        }
    }
}

/// Résultat d'une capture de N trames.
#[derive(Debug, Clone, Serialize)]
pub struct CaptureRun {
    /// Statistiques des trames reçues.
    pub summary: CaptureSummary,
    /// Erreur ayant interrompu la capture avant N trames, le cas échéant.
    pub stopped_by: Option<ErrnoKind>,
    /// Copie de la première trame complète (pour `--save`), non sérialisée.
    #[serde(skip)]
    pub sample: Option<Vec<u8>>,
}

/// Capture `count` trames et mesure cadence, tailles et validité JPEG.
///
/// S'arrête à la première erreur : avec `v4l` 0.14, un flux en erreur n'est pas
/// réutilisable de façon fiable (voir `timeout_behaviour`).
pub fn capture_frames<S: FrameStream>(stream: &mut S, count: u32, keep_sample: bool) -> CaptureRun {
    let mut stats = CaptureStats::default();
    let mut sample = None;
    let mut stopped_by = None;
    for _ in 0..count {
        match stream.next_frame() {
            Ok(frame) => {
                let payload = frame.payload();
                let info = jpeg::inspect(payload);
                stats.record(
                    Instant::now(),
                    frame.buffer.len(),
                    payload.len(),
                    frame.sequence,
                    info,
                );
                if keep_sample && sample.is_none() && info.is_complete() {
                    // Seule copie volontaire du spike : une trame pour inspection manuelle.
                    sample = Some(payload.to_vec());
                }
            }
            Err(error) => {
                stopped_by = Some(ErrnoKind::classify(&error));
                break;
            }
        }
    }
    CaptureRun {
        summary: stats.summary(),
        stopped_by,
        sample,
    }
}

/// Résultat d'un appel à `next_frame`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum CallOutcome {
    /// Une trame a été reçue.
    Frame {
        /// Taille utile.
        bytes_used: usize,
    },
    /// L'appel a échoué.
    Error {
        /// Classification de l'erreur.
        errno: ErrnoKind,
    },
}

/// Comportement du flux après un `TimedOut` (architecture § 3.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TimeoutOutcome {
    /// Trames encore disponibles consommées avant d'obtenir le délai dépassé.
    pub drained_frames: u32,
    /// Un `TimedOut` a bien été provoqué.
    pub timed_out: bool,
    /// Résultats des appels suivants, avec le délai normal.
    pub after_timeout: Vec<CallOutcome>,
}

impl TimeoutOutcome {
    /// Le flux a de nouveau produit une trame après le délai dépassé.
    pub fn recovered(&self) -> bool {
        self.after_timeout
            .iter()
            .any(|outcome| matches!(outcome, CallOutcome::Frame { .. }))
    }
}

/// Provoque un `TimedOut` puis observe les appels suivants.
///
/// Le flux doit déjà avoir produit au moins une trame (flux actif). On lit avec un
/// délai très court jusqu'à vider la file du pilote et obtenir `TimedOut`
/// (au plus `max_drain` trames), puis on effectue `retries` appels avec le délai
/// normal. Hypothèse à vérifier : `EINVAL` à chaque appel suivant (`QBUF` d'un
/// tampon déjà en file), donc flux inutilisable.
pub fn timeout_behaviour<S: FrameStream>(
    stream: &mut S,
    short: Duration,
    normal: Duration,
    max_drain: u32,
    retries: u32,
) -> TimeoutOutcome {
    stream.set_timeout(short);
    let mut drained_frames = 0;
    let mut timed_out = false;
    while drained_frames < max_drain {
        match stream.next_frame() {
            Ok(_) => drained_frames += 1,
            Err(error) => {
                timed_out = ErrnoKind::classify(&error) == ErrnoKind::TimedOut;
                break;
            }
        }
    }
    stream.set_timeout(normal);
    let mut after_timeout = Vec::new();
    if timed_out {
        for _ in 0..retries {
            let outcome = match stream.next_frame() {
                Ok(frame) => CallOutcome::Frame {
                    bytes_used: frame.payload().len(),
                },
                Err(error) => CallOutcome::Error {
                    errno: ErrnoKind::classify(&error),
                },
            };
            after_timeout.push(outcome);
        }
    }
    TimeoutOutcome {
        drained_frames,
        timed_out,
        after_timeout,
    }
}

/// Mesure d'un cycle ouverture → première trame → fermeture.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CycleResult {
    /// Numéro du cycle (à partir de 1).
    pub index: u32,
    /// Latence de démarrage, si le flux a produit une trame.
    pub startup: Option<StartupLatency>,
    /// Durée de la fermeture (`STREAMOFF`, `munmap`, `close`), en millisecondes.
    pub close_ms: Option<f64>,
    /// Erreur rencontrée pendant le cycle.
    pub error: Option<String>,
}

/// Enchaîne `cycles` ouvertures/fermetures pour mesurer le coût d'une réouverture
/// (capture à la demande, reconnexion après `Timeout`).
pub fn reopen_cycles<O: StreamOpener>(
    opener: &mut O,
    cycles: u32,
    timeout: Duration,
) -> Vec<CycleResult> {
    (1..=cycles)
        .map(|index| match open_until_first_frame(opener, timeout) {
            Ok((stream, startup)) => {
                let (close_ms, error) = timed_drop(stream);
                CycleResult {
                    index,
                    startup: Some(startup),
                    close_ms: Some(close_ms),
                    error,
                }
            }
            Err(error) => CycleResult {
                index,
                startup: None,
                close_ms: None,
                error: Some(error_chain(&error)),
            },
        })
        .collect()
}

/// Détruit un flux en mesurant la durée, et capture une éventuelle panique.
///
/// Le `Drop` de `v4l::io::mmap::Stream` panique si `STREAMOFF` échoue avec une
/// erreur autre que `ENODEV` : le spike doit l'observer et le rapporter plutôt
/// que s'interrompre. `AssertUnwindSafe` est sans risque ici : le flux est
/// consommé et aucun état partagé n'est observé après la panique.
///
/// Limite : si un second `Drop` panique pendant le déroulement de la pile (par
/// exemple l'`Arena` de `v4l` après un `STREAMOFF` en échec), Rust interrompt le
/// processus (`abort`) ; `catch_unwind` ne peut pas l'empêcher.
pub fn timed_drop<S>(stream: S) -> (f64, Option<String>) {
    let started = Instant::now();
    let result = panic::catch_unwind(AssertUnwindSafe(move || drop(stream)));
    let elapsed = millis(started.elapsed());
    let error = result.err().map(|payload| {
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or("non-string panic payload");
        format!("drop panicked: {message}")
    });
    (elapsed, error)
}

/// Concatène une erreur et ses causes sur une ligne.
pub fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::fake::{ScriptedOpener, ScriptedStream, Step};
    use std::collections::VecDeque;

    const NORMAL: Duration = Duration::from_secs(2);
    const SHORT: Duration = Duration::from_millis(1);

    fn frame() -> Step {
        Step::Frame {
            used: 1000,
            buffer: 4096,
        }
    }

    #[test]
    fn capture_truncates_to_bytes_used_and_keeps_a_sample() {
        let mut stream = ScriptedStream::new([frame(), frame(), frame()]);
        let run = capture_frames(&mut stream, 3, true);
        assert_eq!(run.summary.frames, 3);
        assert_eq!(run.summary.max_bytes, 1000);
        assert_eq!(run.summary.buffer_len, 4096);
        assert_eq!(run.summary.incomplete_jpeg, 0);
        assert_eq!(run.stopped_by, None);
        assert_eq!(run.sample.map(|s| s.len()), Some(1000));
    }

    #[test]
    fn capture_stops_on_first_error() {
        let mut stream = ScriptedStream::new([frame(), Step::Errno(libc::ENODEV), frame()]);
        let run = capture_frames(&mut stream, 10, false);
        assert_eq!(run.summary.frames, 1);
        assert_eq!(run.stopped_by, Some(ErrnoKind::Enodev));
        assert!(run.sample.is_none());
    }

    #[test]
    fn timeout_then_einval_is_reported_as_not_recovered() {
        // Comportement attendu de v4l 0.14 : deux trames en file, délai dépassé,
        // puis EINVAL à chaque appel.
        let mut stream = ScriptedStream::new([
            frame(),
            frame(),
            Step::TimedOut,
            Step::Errno(libc::EINVAL),
            Step::Errno(libc::EINVAL),
        ]);
        let outcome = timeout_behaviour(&mut stream, SHORT, NORMAL, 50, 2);
        assert_eq!(outcome.drained_frames, 2);
        assert!(outcome.timed_out);
        assert_eq!(
            outcome.after_timeout,
            vec![
                CallOutcome::Error {
                    errno: ErrnoKind::Einval
                };
                2
            ]
        );
        assert!(!outcome.recovered());
        assert_eq!(stream.timeouts, vec![SHORT, NORMAL]);
    }

    #[test]
    fn timeout_followed_by_frame_is_reported_as_recovered() {
        let mut stream = ScriptedStream::new([Step::TimedOut, frame()]);
        let outcome = timeout_behaviour(&mut stream, SHORT, NORMAL, 50, 1);
        assert!(outcome.recovered());
    }

    #[test]
    fn no_timeout_when_frames_never_run_out() {
        let steps: Vec<Step> = (0..10).map(|_| frame()).collect();
        let mut stream = ScriptedStream::new(steps);
        let outcome = timeout_behaviour(&mut stream, SHORT, NORMAL, 3, 2);
        assert!(!outcome.timed_out);
        // Au plus `max_drain` trames consommées.
        assert_eq!(outcome.drained_frames, 3);
        assert!(outcome.after_timeout.is_empty());
    }

    #[test]
    fn reopen_cycles_report_successes_and_failures() {
        let mut opener = ScriptedOpener {
            scripts: VecDeque::from([
                Some(vec![frame()]),
                None,
                Some(vec![Step::TimedOut]),
                Some(vec![frame()]),
            ]),
            opened: 0,
        };
        let results = reopen_cycles(&mut opener, 4, NORMAL);
        assert_eq!(results.len(), 4);
        assert!(results[0].startup.is_some() && results[0].error.is_none());
        assert_eq!(
            results[1].error.as_deref(),
            Some("cannot open stream: scripted open failure")
        );
        assert_eq!(
            results[2].error.as_deref(),
            Some("no first frame (TIMED_OUT): VIDIOC_DQBUF")
        );
        assert!(results[3].close_ms.is_some());
        assert_eq!(opener.opened, 3);
    }

    #[test]
    fn first_frame_error_mentions_drop_panic() {
        let error: StartupError<io::Error> = StartupError::FirstFrame {
            kind: ErrnoKind::Enodev,
            source: io::Error::from_raw_os_error(libc::ENODEV),
            drop_panic: Some("drop panicked: EIO".to_owned()),
        };
        assert_eq!(
            error.to_string(),
            "no first frame (ENODEV), then drop panicked: EIO"
        );
    }

    #[test]
    fn panicking_drop_is_captured() {
        struct Exploding;
        impl Drop for Exploding {
            fn drop(&mut self) {
                panic!("STREAMOFF failed");
            }
        }
        // Le hook par défaut afficherait la panique attendue sur stderr : on le masque.
        let hook = panic::take_hook();
        panic::set_hook(Box::new(|_| {}));
        let (_, error) = timed_drop(Exploding);
        panic::set_hook(hook);
        assert_eq!(error.as_deref(), Some("drop panicked: STREAMOFF failed"));
    }
}
