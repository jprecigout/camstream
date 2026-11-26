//! Rapport du spike : structure sérialisable (`--json`) et rendu texte lisible.

use std::fmt::Write as _;

use serde::Serialize;

use crate::device::{DeviceInfo, FormatInfo, Negotiated};
use crate::probe::{CallOutcome, CaptureRun, CycleResult, StartupLatency, TimeoutOutcome};

/// Paramètres demandés, rappelés dans le rapport.
#[derive(Debug, Clone, Serialize)]
pub struct RequestInfo {
    /// Largeur demandée.
    pub width: u32,
    /// Hauteur demandée.
    pub height: u32,
    /// Cadence demandée.
    pub fps: u32,
    /// Nombre de trames à capturer.
    pub frames: u32,
    /// Nombre de tampons mmap.
    pub buffers: u32,
    /// Délai de lecture normal, en millisecondes.
    pub timeout_ms: u64,
}

/// Minimum, moyenne et maximum d'une série de mesures.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Spread {
    /// Valeur minimale.
    pub min: f64,
    /// Moyenne.
    pub mean: f64,
    /// Valeur maximale.
    pub max: f64,
}

impl Spread {
    /// Calcule la dispersion ; `None` pour une série vide.
    pub fn of(values: impl IntoIterator<Item = f64>) -> Option<Self> {
        let mut count = 0usize;
        let mut sum = 0.0;
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        for value in values {
            count += 1;
            sum += value;
            min = min.min(value);
            max = max.max(value);
        }
        (count > 0).then(|| Self {
            min,
            mean: sum / count as f64,
            max,
        })
    }
}

/// Résultat du test de délai dépassé, avec sa conclusion.
#[derive(Debug, Clone, Serialize)]
pub struct TimeoutReport {
    /// Observations brutes.
    #[serde(flatten)]
    pub outcome: TimeoutOutcome,
    /// Le flux a de nouveau produit une trame.
    pub recovered: bool,
}

impl From<TimeoutOutcome> for TimeoutReport {
    fn from(outcome: TimeoutOutcome) -> Self {
        let recovered = outcome.recovered();
        Self { outcome, recovered }
    }
}

/// Résultat des cycles de réouverture.
#[derive(Debug, Clone, Serialize)]
pub struct ReopenReport {
    /// Détail par cycle.
    pub cycles: Vec<CycleResult>,
    /// Dispersion de la latence ouverture → première trame (cycles réussis).
    pub startup_total_ms: Option<Spread>,
    /// Dispersion de la durée de fermeture.
    pub close_ms: Option<Spread>,
}

impl From<Vec<CycleResult>> for ReopenReport {
    fn from(cycles: Vec<CycleResult>) -> Self {
        let startup_total_ms =
            Spread::of(cycles.iter().filter_map(|c| c.startup.map(|s| s.total_ms)));
        let close_ms = Spread::of(cycles.iter().filter_map(|c| c.close_ms));
        Self {
            cycles,
            startup_total_ms,
            close_ms,
        }
    }
}

/// Rapport complet d'une exécution.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// Version du spike.
    pub probe_version: &'static str,
    /// Nœud sondé.
    pub device: String,
    /// Identité du périphérique.
    pub info: Option<DeviceInfo>,
    /// Formats proposés.
    pub formats: Vec<FormatInfo>,
    /// Paramètres demandés.
    pub request: RequestInfo,
    /// Format négocié.
    pub negotiated: Option<Negotiated>,
    /// Latence de démarrage à froid (première ouverture).
    pub startup: Option<StartupLatency>,
    /// Capture de N trames.
    pub capture: Option<CaptureRun>,
    /// Comportement après `TimedOut`.
    pub timeout_test: Option<TimeoutReport>,
    /// Durée de fermeture du flux principal, après le test de délai.
    pub close_after_timeout_ms: Option<f64>,
    /// Cycles de réouverture.
    pub reopen: Option<ReopenReport>,
    /// Trame enregistrée sur disque.
    pub saved_frame: Option<String>,
    /// Erreurs non fatales rencontrées.
    pub errors: Vec<String>,
}

impl Report {
    /// Rapport vide pour un périphérique et des paramètres donnés.
    pub fn new(device: String, request: RequestInfo) -> Self {
        Self {
            probe_version: env!("CARGO_PKG_VERSION"),
            device,
            info: None,
            formats: Vec::new(),
            request,
            negotiated: None,
            startup: None,
            capture: None,
            timeout_test: None,
            close_after_timeout_ms: None,
            reopen: None,
            saved_frame: None,
            errors: Vec::new(),
        }
    }
}

/// Valeur optionnelle formatée, ou « n/a ».
fn opt(value: Option<f64>, decimals: usize) -> String {
    value.map_or_else(|| "n/a".to_owned(), |v| format!("{v:.decimals$}"))
}

/// Rendu texte lisible du rapport.
///
/// `write!` vers une `String` ne peut pas échouer : les résultats sont ignorés.
pub fn render_text(report: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "== v4l-probe {} : {}",
        report.probe_version, report.device
    );
    if let Some(info) = &report.info {
        let _ = writeln!(
            out,
            "card: {} | driver: {} | bus: {}",
            info.card, info.driver, info.bus
        );
    }

    let _ = writeln!(out, "\n-- formats");
    for format in &report.formats {
        let _ = writeln!(out, "{} ({})", format.fourcc, format.description);
        for size in &format.sizes {
            let _ = writeln!(out, "  {:<12} fps: {}", size.size, size.fps.join(", "));
        }
    }

    let request = &report.request;
    let _ = writeln!(
        out,
        "\n-- request: MJPG {}x{} @ {} fps, {} frames, {} buffers, timeout {} ms",
        request.width,
        request.height,
        request.fps,
        request.frames,
        request.buffers,
        request.timeout_ms
    );
    if let Some(n) = &report.negotiated {
        let _ = writeln!(
            out,
            "negotiated: {} {}x{} interval {} ({} fps), sizeimage {} B",
            n.fourcc,
            n.width,
            n.height,
            n.interval,
            opt(n.fps, 2),
            n.size_image
        );
    }

    if let Some(s) = &report.startup {
        let _ = writeln!(
            out,
            "\n-- cold start: total {:.1} ms (open {:.1}, configure {:.1}, buffers {:.1}, first frame {:.1})",
            s.total_ms, s.open_ms, s.configure_ms, s.buffers_ms, s.first_frame_ms
        );
    }

    if let Some(run) = &report.capture {
        let c = &run.summary;
        let _ = writeln!(
            out,
            "\n-- capture: {} frames in {:.0} ms, {} fps",
            c.frames,
            c.span_ms,
            opt(c.fps, 2)
        );
        let _ = writeln!(
            out,
            "size: mean {} B, min {}, max {} | mmap buffer {} B",
            opt(c.mean_bytes, 0),
            c.min_bytes
                .map_or_else(|| "n/a".to_owned(), |v| v.to_string()),
            c.max_bytes,
            c.buffer_len
        );
        let _ = writeln!(
            out,
            "jpeg: incomplete {} | without DHT {} | zero padding {} | driver gaps {}",
            c.incomplete_jpeg, c.without_dht, c.padded, c.sequence_gaps
        );
        if let Some(kind) = run.stopped_by {
            let _ = writeln!(out, "capture stopped early: {kind}");
        }
    }

    if let Some(t) = &report.timeout_test {
        let after: Vec<String> = t
            .outcome
            .after_timeout
            .iter()
            .map(|o| match o {
                CallOutcome::Frame { bytes_used } => format!("frame({bytes_used} B)"),
                CallOutcome::Error { errno } => errno.to_string(),
            })
            .collect();
        let _ = writeln!(
            out,
            "\n-- timeout test: timed out {} after draining {} frames | next calls: [{}] | recovered: {}",
            t.outcome.timed_out,
            t.outcome.drained_frames,
            after.join(", "),
            t.recovered
        );
    }
    if let Some(close) = report.close_after_timeout_ms {
        let _ = writeln!(out, "close after timeout test: {close:.1} ms");
    }

    if let Some(reopen) = &report.reopen {
        let _ = writeln!(out, "\n-- reopen cycles: {}", reopen.cycles.len());
        for cycle in &reopen.cycles {
            let _ = writeln!(
                out,
                "  #{:<2} start {} ms | close {} ms{}",
                cycle.index,
                opt(cycle.startup.map(|s| s.total_ms), 1),
                opt(cycle.close_ms, 1),
                cycle
                    .error
                    .as_deref()
                    .map_or_else(String::new, |e| format!(" | error: {e}"))
            );
        }
        if let Some(s) = reopen.startup_total_ms {
            let _ = writeln!(
                out,
                "  start ms: min {:.1} / mean {:.1} / max {:.1}",
                s.min, s.mean, s.max
            );
        }
        if let Some(s) = reopen.close_ms {
            let _ = writeln!(
                out,
                "  close ms: min {:.1} / mean {:.1} / max {:.1}",
                s.min, s.mean, s.max
            );
        }
    }

    if let Some(path) = &report.saved_frame {
        let _ = writeln!(out, "\nsaved frame: {path}");
    }
    if !report.errors.is_empty() {
        let _ = writeln!(out, "\n-- errors");
        for error in &report.errors {
            let _ = writeln!(out, "  {error}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrnoKind;

    fn request() -> RequestInfo {
        RequestInfo {
            width: 640,
            height: 480,
            fps: 15,
            frames: 100,
            buffers: 4,
            timeout_ms: 2000,
        }
    }

    #[test]
    fn spread_of_empty_series_is_none() {
        assert_eq!(Spread::of(std::iter::empty()), None);
    }

    #[test]
    fn spread_computes_min_mean_max() {
        let spread = Spread::of([3.0, 1.0, 2.0]).expect("série non vide");
        assert_eq!(
            spread,
            Spread {
                min: 1.0,
                mean: 2.0,
                max: 3.0
            }
        );
    }

    #[test]
    fn timeout_report_serializes_flat_with_conclusion() {
        let report = TimeoutReport::from(TimeoutOutcome {
            drained_frames: 3,
            timed_out: true,
            after_timeout: vec![CallOutcome::Error {
                errno: ErrnoKind::Einval,
            }],
        });
        let json = serde_json::to_value(&report).expect("sérialisable");
        assert_eq!(json["timed_out"], true);
        assert_eq!(json["recovered"], false);
        assert_eq!(json["after_timeout"][0]["result"], "error");
        assert_eq!(json["after_timeout"][0]["errno"], "EINVAL");
    }

    #[test]
    fn text_rendering_of_minimal_report_mentions_device_and_errors() {
        let mut report = Report::new("/dev/video0".to_owned(), request());
        report.errors.push("startup: cannot open stream".to_owned());
        let text = render_text(&report);
        assert!(text.contains("/dev/video0"));
        assert!(text.contains("MJPG 640x480 @ 15 fps"));
        assert!(text.contains("startup: cannot open stream"));
    }
}
