//! Spike `v4l-probe` : mesure le comportement réel de la crate `v4l` 0.14 sur une
//! webcam (formats, négociation MJPEG, latence de démarrage, cadence, taille des
//! trames, comportement après `TimedOut`, coût des réouvertures).
//!
//! Outil jetable, isolé de l'application (voir `docs/spikes/0001-v4l.md`). Le rapport
//! est écrit sur la sortie standard (c'est le produit de l'outil) ; les journaux
//! `tracing` vont sur la sortie d'erreur.

#![forbid(unsafe_code)]

mod device;
mod error;
mod jpeg;
mod probe;
mod report;
mod stats;
mod stream;

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use tracing::{info, warn};

use crate::device::{CaptureRequest, V4l2Opener};
use crate::error::ProbeError;
use crate::probe::error_chain;
use crate::report::{ReopenReport, Report, RequestInfo, TimeoutReport};

/// Arguments de la ligne de commande (les commentaires `///` servent d'aide `--help`).
#[derive(Debug, Parser)]
#[command(
    version,
    about = "Sonde une webcam V4L2 avec la crate v4l (spike Camstream)"
)]
struct Cli {
    /// Nœud V4L2 de capture.
    #[arg(long, default_value = "/dev/video0")]
    device: PathBuf,
    /// Résolution demandée, LARGEURxHAUTEUR.
    #[arg(long, default_value = "640x480", value_parser = parse_resolution)]
    resolution: (u32, u32),
    /// Cadence demandée (images par seconde).
    #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u32).range(1..=240))]
    fps: u32,
    /// Nombre de trames capturées pour les statistiques de cadence et de taille.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..))]
    frames: u32,
    /// Délai de lecture normal en millisecondes (`MmapStream::set_timeout`).
    #[arg(long, default_value_t = 2000, value_parser = clap::value_parser!(u64).range(1..))]
    timeout_ms: u64,
    /// Délai très court utilisé pour provoquer `TimedOut`, en millisecondes.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u64).range(1..))]
    short_timeout_ms: u64,
    /// Nombre de tampons mmap.
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u32).range(1..=32))]
    buffers: u32,
    /// Nombre de cycles ouverture / première trame / fermeture (0 : aucun).
    #[arg(long, default_value_t = 5)]
    cycles: u32,
    /// Ne pas exécuter le test de comportement après `TimedOut`.
    #[arg(long)]
    skip_timeout_test: bool,
    /// Afficher seulement l'identité du périphérique et ses formats.
    #[arg(long)]
    list_only: bool,
    /// Enregistrer la première trame complète dans ce fichier JPEG.
    #[arg(long)]
    save: Option<PathBuf>,
    /// Produire le rapport en JSON plutôt qu'en texte.
    #[arg(long)]
    json: bool,
}

/// Analyse « LARGEURxHAUTEUR » (ex. « 640x480 »), dimensions non nulles.
fn parse_resolution(text: &str) -> Result<(u32, u32), String> {
    let (width, height) = text
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("expected WIDTHxHEIGHT, got {text:?}"))?;
    let parse = |part: &str| {
        part.trim()
            .parse::<u32>()
            .ok()
            .filter(|&value| value > 0)
            .ok_or_else(|| format!("invalid dimension {part:?} in {text:?}"))
    };
    Ok((parse(width)?, parse(height)?))
}

fn main() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let report = run(&cli)?;

    let mut stdout = std::io::stdout().lock();
    if cli.json {
        serde_json::to_writer_pretty(&mut stdout, &report).context("cannot write JSON report")?;
        writeln!(stdout).context("cannot write report")?;
    } else {
        write!(stdout, "{}", report::render_text(&report)).context("cannot write report")?;
    }

    Ok(if report.errors.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Enchaîne les mesures. Les échecs d'étape sont consignés dans le rapport ; seule
/// l'impossibilité d'ouvrir le périphérique est fatale.
fn run(cli: &Cli) -> anyhow::Result<Report> {
    let (width, height) = cli.resolution;
    let timeout = Duration::from_millis(cli.timeout_ms);
    let mut report = Report::new(
        cli.device.display().to_string(),
        RequestInfo {
            width,
            height,
            fps: cli.fps,
            frames: cli.frames,
            buffers: cli.buffers,
            timeout_ms: cli.timeout_ms,
        },
    );

    report.info = Some(
        device::query_info(&cli.device)
            .with_context(|| format!("cannot query {}", cli.device.display()))?,
    );
    report.formats = device::list_formats(&cli.device).context("cannot list formats")?;
    if cli.list_only {
        return Ok(report);
    }

    let mut opener = V4l2Opener::new(CaptureRequest {
        path: cli.device.clone(),
        width,
        height,
        fps: cli.fps,
        buffers: cli.buffers,
    });

    info!("cold start: open, negotiate MJPEG, first frame");
    let (mut stream, startup) = match probe::open_until_first_frame(&mut opener, timeout) {
        Ok(started) => started,
        Err(error) => {
            report.negotiated = opener.negotiated.clone();
            report
                .errors
                .push(format!("startup: {}", error_chain(&error)));
            return Ok(report);
        }
    };
    report.negotiated = opener.negotiated.clone();
    report.startup = Some(startup);

    info!(frames = cli.frames, "capturing");
    let run = probe::capture_frames(&mut stream, cli.frames, cli.save.is_some());
    if let (Some(path), Some(sample)) = (&cli.save, &run.sample) {
        match std::fs::write(path, sample) {
            Ok(()) => report.saved_frame = Some(path.display().to_string()),
            Err(source) => {
                let error = ProbeError::SaveFrame {
                    path: path.clone(),
                    source,
                };
                report.errors.push(error_chain(&error));
            }
        }
    }
    if let Some(kind) = run.stopped_by {
        report.errors.push(format!("capture stopped early: {kind}"));
    }
    report.capture = Some(run);

    // Un flux interrompu par une erreur (débranchement, `TimedOut`…) n'est plus dans
    // l'état attendu par le test de délai : ses résultats seraient trompeurs.
    let capture_failed = report
        .capture
        .as_ref()
        .is_some_and(|run| run.stopped_by.is_some());
    if cli.skip_timeout_test || capture_failed {
        if capture_failed && !cli.skip_timeout_test {
            warn!("capture stopped early: timeout test skipped");
        }
        // Destruction via `timed_drop` : une panique du `Drop` est rapportée.
        let (_, error) = probe::timed_drop(stream);
        if let Some(error) = error {
            report.errors.push(format!("close after capture: {error}"));
        }
    } else {
        info!("timeout test: provoking TimedOut, then observing next calls");
        let outcome = probe::timeout_behaviour(
            &mut stream,
            Duration::from_millis(cli.short_timeout_ms),
            timeout,
            // Au moins deux fois le nombre de tampons (8 au minimum) : au-delà, la file
            // du pilote ne peut plus contenir de trame déjà prête.
            cli.buffers.saturating_mul(2).max(8),
            3,
        );
        if !outcome.timed_out {
            warn!("could not provoke TimedOut; try a smaller --short-timeout-ms");
        }
        report.timeout_test = Some(TimeoutReport::from(outcome));
        let (close_ms, error) = probe::timed_drop(stream);
        report.close_after_timeout_ms = Some(close_ms);
        if let Some(error) = error {
            report
                .errors
                .push(format!("close after timeout test: {error}"));
        }
    }

    if cli.cycles > 0 {
        info!(cycles = cli.cycles, "reopen cycles");
        let cycles = probe::reopen_cycles(&mut opener, cli.cycles, timeout);
        report.errors.extend(
            cycles
                .iter()
                .filter_map(|c| c.error.as_ref().map(|e| format!("cycle {}: {e}", c.index))),
        );
        report.reopen = Some(ReopenReport::from(cycles));
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_resolutions() {
        assert_eq!(parse_resolution("640x480"), Ok((640, 480)));
        assert_eq!(parse_resolution("1280X720"), Ok((1280, 720)));
    }

    #[test]
    fn rejects_invalid_resolutions() {
        for text in ["640", "0x480", "640x", "axb", "640x480x2", ""] {
            assert!(parse_resolution(text).is_err(), "{text:?} accepted");
        }
    }

    #[test]
    fn cli_defaults_match_v1_targets() {
        let cli = Cli::try_parse_from(["v4l-probe"]).expect("defaults are valid");
        assert_eq!(cli.resolution, (640, 480));
        assert_eq!(cli.fps, 15);
        assert_eq!(cli.timeout_ms, 2000);
        assert_eq!(cli.device, PathBuf::from("/dev/video0"));
    }

    #[test]
    fn cli_definition_is_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
