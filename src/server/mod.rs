//! Adaptateur HTTP (axum) : page d'accueil, flux MJPEG et santé.
//!
//! Implémente le pattern **Adapter** (adaptateur primaire de l'architecture
//! hexagonale) : traduit les requêtes HTTP en abonnements au `FrameHub` et l'état
//! `SourceStatus` en JSON, sans que le cœur connaisse axum. Dépend du cœur
//! (`stream`, `domain`) mais jamais de `capture` (ADR 0001).

pub mod health;
pub mod mjpeg;
pub mod net;
mod routes;

use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use axum::serve::ListenerExt;
use axum::Router;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

pub use health::{CameraReport, HealthReport, HealthState, HealthcheckError, SourceReport};
pub use mjpeg::MaxClients;
pub use net::TcpOptions;

use crate::stream::{FrameHub, StatusReceiver};
use mjpeg::ClientLimiter;
use routes::ServerState;

/// Erreurs du serveur HTTP.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// Paramètre obligatoire absent du builder.
    #[error("missing server parameter: {0}")]
    MissingParameter(&'static str),
    /// Impossible d'écouter sur l'adresse (port occupé, droits).
    #[error("cannot bind {addr}")]
    Bind {
        /// Adresse demandée.
        addr: SocketAddr,
        /// Erreur d'origine.
        #[source]
        source: std::io::Error,
    },
    /// Erreur fatale pendant le service.
    #[error("HTTP server error")]
    Serve(#[source] std::io::Error),
}

/// Construit le serveur HTTP étape par étape.
///
/// Implémente le pattern **Builder** : paramètres obligatoires (adresse, hub,
/// état, jeton d'arrêt) vérifiés par `build`, paramètres optionnels avec défauts
/// (`MaxClients`, options TCP).
#[derive(Debug)]
pub struct ServerBuilder {
    bind: SocketAddr,
    hub: Option<FrameHub>,
    status: Option<StatusReceiver>,
    max_clients: MaxClients,
    tcp_options: TcpOptions,
    shutdown: Option<CancellationToken>,
}

impl ServerBuilder {
    /// Démarre la construction pour l'adresse d'écoute donnée (port 0 : port libre
    /// choisi par le système, utile aux tests).
    pub fn new(bind: SocketAddr) -> Self {
        Self {
            bind,
            hub: None,
            status: None,
            max_clients: MaxClients::DEFAULT,
            tcp_options: TcpOptions::default(),
            shutdown: None,
        }
    }

    /// Source des trames diffusées (obligatoire).
    #[must_use]
    pub fn hub(mut self, hub: FrameHub) -> Self {
        self.hub = Some(hub);
        self
    }

    /// État de la capture exposé par `/health` (obligatoire).
    #[must_use]
    pub fn status(mut self, status: StatusReceiver) -> Self {
        self.status = Some(status);
        self
    }

    /// Limite de clients simultanés (défaut : 10).
    #[must_use]
    pub fn max_clients(mut self, max: MaxClients) -> Self {
        self.max_clients = max;
        self
    }

    /// Détection des clients disparus (défaut : keepalive 30 s, `TCP_USER_TIMEOUT` 30 s).
    #[must_use]
    pub fn tcp_options(mut self, options: TcpOptions) -> Self {
        self.tcp_options = options;
        self
    }

    /// Jeton déclenchant l'arrêt gracieux (fin des flux en cours ; obligatoire).
    #[must_use]
    pub fn shutdown(mut self, token: CancellationToken) -> Self {
        self.shutdown = Some(token);
        self
    }

    /// Valide et construit le serveur.
    pub fn build(self) -> Result<Server, ServerError> {
        let hub = self.hub.ok_or(ServerError::MissingParameter("hub"))?;
        let status = self.status.ok_or(ServerError::MissingParameter("status"))?;
        let shutdown = self
            .shutdown
            .ok_or(ServerError::MissingParameter("shutdown"))?;
        let router = routes::router(ServerState {
            hub,
            status,
            limiter: ClientLimiter::new(self.max_clients),
            shutdown: shutdown.clone(),
            next_client_id: Arc::new(AtomicU64::new(1)),
        });
        Ok(Server {
            bind: self.bind,
            router,
            tcp_options: self.tcp_options,
            shutdown,
        })
    }
}

/// Serveur HTTP prêt à démarrer.
///
/// Façade de l'adaptateur HTTP (pattern **Adapter**, voir le module).
#[derive(Debug)]
pub struct Server {
    bind: SocketAddr,
    router: Router,
    tcp_options: TcpOptions,
    shutdown: CancellationToken,
}

impl Server {
    /// Routeur axum, exposé pour les tests sans socket (`tower::ServiceExt::oneshot`).
    pub fn router(&self) -> Router {
        self.router.clone()
    }

    /// Réserve l'adresse d'écoute ; l'erreur (port occupé) remonte avant tout service.
    pub async fn bind(self) -> Result<BoundServer, ServerError> {
        let listener = TcpListener::bind(self.bind)
            .await
            .map_err(|source| ServerError::Bind {
                addr: self.bind,
                source,
            })?;
        let local_addr = listener.local_addr().map_err(|source| ServerError::Bind {
            addr: self.bind,
            source,
        })?;
        Ok(BoundServer {
            listener,
            local_addr,
            router: self.router,
            tcp_options: self.tcp_options,
            shutdown: self.shutdown,
        })
    }

    /// Écoute et sert jusqu'à l'annulation du jeton.
    pub async fn run(self) -> Result<(), ServerError> {
        self.bind().await?.serve().await
    }
}

/// Serveur dont l'adresse d'écoute est réservée.
#[derive(Debug)]
pub struct BoundServer {
    listener: TcpListener,
    local_addr: SocketAddr,
    router: Router,
    tcp_options: TcpOptions,
    shutdown: CancellationToken,
}

impl BoundServer {
    /// Adresse effective (port réel si le port demandé était 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Sert les requêtes jusqu'à l'annulation du jeton, puis attend la fin des
    /// connexions en cours (les flux MJPEG se terminent d'eux-mêmes sur le jeton).
    ///
    /// L'appelant doit borner cette attente (`shutdown_timeout`) : un client bloqué en
    /// écriture n'observe pas le jeton.
    pub async fn serve(self) -> Result<(), ServerError> {
        let options = self.tcp_options;
        let listener = self.listener.tap_io(move |stream| {
            // Erreur non fatale : la connexion reste servie sans les options.
            if let Err(err) = net::apply_tcp_options(socket2::SockRef::from(&*stream), &options) {
                tracing::debug!(%err, "cannot set TCP options on accepted connection");
            }
        });
        tracing::info!(addr = %self.local_addr, "HTTP server listening");
        axum::serve(
            listener,
            self.router
                .into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(self.shutdown.cancelled_owned())
        .await
        .map_err(ServerError::Serve)
    }
}
