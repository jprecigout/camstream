//! Point d'entrée du binaire `camstream` : configuration, logs, runtime tokio,
//! signaux `SIGTERM`/`SIGINT` et code de sortie.
//!
//! Codes de sortie : 0 arrêt normal, 1 échec (capture, serveur, healthcheck),
//! 2 configuration invalide (clap utilise aussi 2 pour ses erreurs d'analyse).

#![forbid(unsafe_code)]

use std::future::Future;
use std::io::IsTerminal;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use camstream::app::App;
use camstream::config::{AppConfig, Command, ConfigError, HealthcheckConfig};
use camstream::server::health;
use tokio::signal::unix::{signal, SignalKind};
use tracing_subscriber::EnvFilter;

/// Délai laissé aux tâches restantes à la destruction du runtime (après l'arrêt
/// gracieux, déjà borné par `CAMSTREAM_SHUTDOWN_TIMEOUT_MS`).
const RUNTIME_SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

fn main() -> ExitCode {
    init_tracing();
    let command = match Command::load() {
        Ok(command) => command,
        // `--help`, `--version` (code 0) et erreurs d'analyse (code 2), affichés par clap.
        Err(ConfigError::Parse(error)) => error.exit(),
        Err(error) => {
            tracing::error!("{error}");
            return ExitCode::from(2);
        }
    };
    match command {
        Command::Healthcheck(config) => healthcheck(config),
        Command::Serve(config) => match serve(config) {
            Ok(outcome) if outcome.is_success() => ExitCode::SUCCESS,
            Ok(_) => ExitCode::FAILURE,
            Err(error) => {
                tracing::error!(error = format!("{error:#}"), "camstream failed");
                ExitCode::FAILURE
            }
        },
    }
}

/// Logs `tracing` filtrés par `RUST_LOG` (défaut : `info`), sur la sortie standard.
///
/// Couleurs ANSI uniquement sur un terminal : `docker logs` reste lisible.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // `try_init` : aucun abonné global n'existe encore, l'échec est impossible ici,
    // mais on évite toute panique en production.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::stdout().is_terminal())
        .try_init();
}

/// Démarre le runtime multi-thread et l'application jusqu'à l'arrêt.
fn serve(config: AppConfig) -> anyhow::Result<camstream::app::Outcome> {
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        source = %config.capture.kind,
        device = %config.capture.device,
        resolution = %config.capture.resolution,
        fps = %config.capture.fps,
        bind = %config.bind,
        "starting camstream"
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("camstream-worker")
        .build()
        .context("cannot start the tokio runtime")?;
    let result = runtime.block_on(async {
        let shutdown = shutdown_signal()?;
        App::new(config).bind().await?.run(shutdown).await
    });
    // Ne pas attendre indéfiniment une tâche bloquante restante (join de la capture
    // expiré) : le processus doit quitter.
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_GRACE);
    result
}

/// Installe les gestionnaires de `SIGTERM` (`docker stop`) et `SIGINT` (Ctrl+C) et
/// renvoie un futur résolu au premier signal reçu.
fn shutdown_signal() -> anyhow::Result<impl Future<Output = ()>> {
    let mut terminate =
        signal(SignalKind::terminate()).context("cannot install the SIGTERM handler")?;
    let mut interrupt =
        signal(SignalKind::interrupt()).context("cannot install the SIGINT handler")?;
    Ok(async move {
        tokio::select! {
            _ = terminate.recv() => tracing::info!("SIGTERM received"),
            _ = interrupt.recv() => tracing::info!("SIGINT received"),
        }
    })
}

/// Sous-commande `healthcheck` : interroge `/health` de l'instance locale.
fn healthcheck(config: HealthcheckConfig) -> ExitCode {
    let target = health::healthcheck_target(config.bind.ip(), config.bind.port());
    match health::check(target, config.timeout) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = format!("{:#}", anyhow::Error::new(error)), %target,
                "healthcheck failed");
            ExitCode::FAILURE
        }
    }
}
