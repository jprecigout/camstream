//! Racine de composition : câble le hub, le superviseur de capture et le serveur
//! HTTP, puis pilote le cycle de vie (démarrage, arrêt gracieux borné, code de sortie).
//!
//! Seul module de la bibliothèque qui connaît tous les autres et utilise `anyhow`
//! (ADR 0001). Les signaux du système sont traduits par `main.rs` en un futur
//! d'arrêt injecté dans [`BoundApp::run`], ce qui rend le cycle de vie testable.

use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{bail, Context};
use tokio::runtime::{Handle, RuntimeFlavor};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::capture::SourceFactory;
use crate::config::AppConfig;
use crate::domain::FrameSourceFactory;
use crate::server::{BoundServer, MaxClients, ServerBuilder, ServerError};
use crate::stream::{
    BackoffPolicy, CaptureHandle, CaptureSupervisor, CaptureThreadError, FrameHub, HubCapacity,
    SourceStatus, StatusReceiver, SupervisorSettings,
};

/// Issue du cycle de vie de l'application, traduite en code de sortie par `main`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Arrêt demandé (`SIGTERM`/`SIGINT`) et terminé : code 0.
    Stopped,
    /// Capture en échec (erreur fatale ou fin anormale du thread) : code 1.
    CaptureFailed,
    /// Le serveur HTTP s'est arrêté de lui-même : code 1.
    ServerFailed,
}

impl Outcome {
    /// Vrai pour un arrêt normal (code de sortie 0).
    pub fn is_success(self) -> bool {
        self == Self::Stopped
    }
}

/// Application non démarrée : configuration et fabrique de sources.
///
/// La fabrique est injectée (pattern **Factory**) : [`App::new`] utilise la fabrique
/// réelle (`SourceFactory`), les tests une fabrique scriptée.
pub struct App {
    config: AppConfig,
    factory: Box<dyn FrameSourceFactory>,
}

impl App {
    /// Application avec la fabrique de sources déduite de la configuration.
    pub fn new(config: AppConfig) -> Self {
        let factory = Box::new(SourceFactory::new(config.capture.clone()));
        Self::with_factory(config, factory)
    }

    /// Application avec une fabrique de sources fournie (tests, intégrations).
    pub fn with_factory(config: AppConfig, factory: Box<dyn FrameSourceFactory>) -> Self {
        Self { config, factory }
    }

    /// Construit les composants et réserve l'adresse d'écoute.
    ///
    /// Doit être appelé dans un runtime tokio **multi-thread** : le thread de capture
    /// attend via `Handle::block_on`, qui ne fait pas avancer les minuteries d'un
    /// runtime `current_thread`.
    pub async fn bind(self) -> anyhow::Result<BoundApp> {
        let runtime = Handle::try_current().context("camstream must run inside a tokio runtime")?;
        if runtime.runtime_flavor() == RuntimeFlavor::CurrentThread {
            bail!("camstream requires a multi-thread tokio runtime");
        }
        let config = self.config;
        let root = CancellationToken::new();
        let hub = FrameHub::new(HubCapacity::from(config.hub_capacity));
        let settings = SupervisorSettings {
            backoff: BackoffPolicy::new(config.reconnect_initial, config.reconnect_max),
            idle_timeout: config.idle_timeout,
            max_corrupted_frames: config.max_corrupted_frames,
        };
        // Jetons enfants : ni le serveur ni la capture ne peuvent arrêter l'application ;
        // seul `run` annule le jeton racine (ADR 0004, décision 5).
        let supervisor = CaptureSupervisor::new(
            self.factory,
            hub.clone(),
            settings,
            runtime,
            root.child_token(),
        );
        let status = supervisor.status();
        let server = ServerBuilder::new(config.bind)
            .hub(hub)
            .status(status.clone())
            .max_clients(MaxClients::from(config.max_clients))
            .shutdown(root.child_token())
            .build()?
            .bind()
            .await
            .context("cannot start the HTTP server")?;
        Ok(BoundApp {
            server,
            supervisor,
            status,
            root,
            shutdown_timeout: config.shutdown_timeout,
        })
    }
}

/// Application dont l'adresse d'écoute est réservée, prête à tourner.
pub struct BoundApp {
    server: BoundServer,
    supervisor: CaptureSupervisor,
    status: StatusReceiver,
    root: CancellationToken,
    shutdown_timeout: Duration,
}

impl BoundApp {
    /// Adresse d'écoute effective (port réel si le port configuré était 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.server.local_addr()
    }

    /// Récepteur de l'état de la capture (observabilité, tests).
    pub fn status(&self) -> StatusReceiver {
        self.status.clone()
    }

    /// Démarre la capture et le serveur, attend l'arrêt, puis arrête tout dans la
    /// limite de `shutdown_timeout`.
    ///
    /// Le `select!` de cette méthode est le **seul** endroit qui décide de l'arrêt
    /// global : futur `shutdown` (signal), état `Failed` de la capture, fin anormale du
    /// thread de capture (canal d'état fermé sans `Failed`, par exemple une panique),
    /// ou fin inattendue du serveur.
    pub async fn run(self, shutdown: impl Future<Output = ()>) -> anyhow::Result<Outcome> {
        let Self {
            server,
            supervisor,
            status,
            root,
            shutdown_timeout,
        } = self;
        let capture = supervisor
            .spawn()
            .context("cannot spawn the capture thread")?;
        let mut server_task = tokio::spawn(server.serve());
        let mut server_finished = false;

        let mut outcome = tokio::select! {
            () = shutdown => {
                tracing::info!("shutdown requested");
                Outcome::Stopped
            }
            reason = capture_failure(status) => {
                tracing::error!(%reason, "capture failed, shutting down");
                Outcome::CaptureFailed
            }
            result = &mut server_task => {
                server_finished = true;
                log_server_end(result);
                Outcome::ServerFailed
            }
        };

        root.cancel();
        let deadline = Instant::now() + shutdown_timeout;
        let (_, capture_end) = tokio::join!(
            stop_server(&mut server_task, server_finished, deadline),
            stop_capture(capture, deadline),
        );
        if capture_end == CaptureEnd::Abnormal {
            outcome = Outcome::CaptureFailed;
        }
        tracing::info!(?outcome, "camstream stopped");
        Ok(outcome)
    }
}

/// Se résout quand la capture échoue, avec la cause : état `Failed`, ou canal
/// d'état fermé sans `Failed` (thread terminé anormalement).
async fn capture_failure(mut status: StatusReceiver) -> String {
    // `wait_for` examine aussi la valeur courante, même si l'émetteur a disparu : un
    // `Failed` publié juste avant la fin du thread est vu. Le `watch::Ref` (verrou)
    // est converti immédiatement en `String`.
    let failure = status
        .wait_for(SourceStatus::is_failed)
        .await
        .map(|current| match &*current {
            SourceStatus::Failed { reason } => reason.clone(),
            other => format!("{other:?}"),
        });
    failure.unwrap_or_else(|_| {
        "capture thread ended unexpectedly without reporting a failure".to_owned()
    })
}

/// Journalise la fin inattendue du serveur HTTP.
fn log_server_end(result: Result<Result<(), ServerError>, tokio::task::JoinError>) {
    match result {
        Ok(Ok(())) => tracing::error!("HTTP server stopped unexpectedly"),
        Ok(Err(error)) => {
            tracing::error!(
                error = format!("{:#}", anyhow::Error::new(error)),
                "HTTP server failed"
            );
        }
        Err(error) => tracing::error!(%error, "HTTP server task failed"),
    }
}

/// Attend la fin gracieuse du serveur (flux terminés sur le jeton) ; au-delà de
/// l'échéance (client bloqué en écriture), la tâche est interrompue.
async fn stop_server(
    task: &mut JoinHandle<Result<(), ServerError>>,
    finished: bool,
    deadline: Instant,
) {
    if finished {
        return;
    }
    match tokio::time::timeout_at(deadline, &mut *task).await {
        Ok(Ok(Ok(()))) => tracing::debug!("HTTP server stopped"),
        Ok(result) => log_server_end(result),
        Err(_) => {
            tracing::warn!("HTTP server did not stop within the shutdown timeout, aborting");
            task.abort();
        }
    }
}

/// Fin du thread de capture vue lors de l'arrêt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureEnd {
    /// Terminé normalement, ou erreur fatale déjà signalée par l'état `Failed`.
    Expected,
    /// Pas terminé dans le délai (lecture bloquée) : journalisé, sans changer l'issue.
    ///
    /// Une lecture bloquée n'est pas un défaut de la capture : la configuration
    /// garantit `read_timeout < shutdown_timeout`, et le processus quitte de toute
    /// façon. Compter ce cas comme un échec donnerait un faux code 1 à l'arrêt.
    TimedOut,
    /// Panique du thread de capture : code 1 même si l'arrêt avait été demandé.
    Abnormal,
}

/// Attend la fin du thread de capture sans bloquer le runtime (`spawn_blocking`),
/// dans la limite de l'échéance.
async fn stop_capture(capture: CaptureHandle, deadline: Instant) -> CaptureEnd {
    let join = tokio::task::spawn_blocking(move || capture.join());
    match tokio::time::timeout_at(deadline, join).await {
        Ok(Ok(Ok(()))) => {
            tracing::debug!("capture thread joined");
            CaptureEnd::Expected
        }
        // Déjà journalisée par le superviseur et observée via `Failed`.
        Ok(Ok(Err(CaptureThreadError::Fatal(_)))) => CaptureEnd::Expected,
        Ok(Ok(Err(error @ CaptureThreadError::Panicked(_)))) => {
            tracing::error!(%error, "capture thread ended abnormally");
            CaptureEnd::Abnormal
        }
        Ok(Err(error)) => {
            tracing::error!(%error, "capture join task failed");
            CaptureEnd::Abnormal
        }
        Err(_) => {
            // Lecture V4L2 bloquée au-delà du délai : le processus va quitter quand même.
            tracing::warn!("capture thread did not stop within the shutdown timeout");
            CaptureEnd::TimedOut
        }
    }
}
