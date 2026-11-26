//! Routeur axum : `/`, `/stream.mjpg`, `/health` (toute autre route : `404`).

use axum::http::header;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;
use tokio_util::sync::CancellationToken;

use super::health;
use super::mjpeg::{self, ClientIds, ClientLimiter};
use crate::stream::{FrameHub, StatusReceiver};

/// Page d'accueil minimale, embarquée dans le binaire.
const INDEX_HTML: &str = include_str!("index.html");

/// État partagé par les handlers (clones peu coûteux : canaux et `Arc`).
///
/// Nommé `ServerState` (et non `AppState`) pour ne pas le confondre avec le module
/// `app`, racine de composition.
#[derive(Debug, Clone)]
pub(crate) struct ServerState {
    /// Source des trames diffusées.
    pub(crate) hub: FrameHub,
    /// État de la capture, lu par `/health`.
    pub(crate) status: StatusReceiver,
    /// Limite de clients de flux simultanés.
    pub(crate) limiter: ClientLimiter,
    /// Jeton d'arrêt observé par chaque flux.
    pub(crate) shutdown: CancellationToken,
    /// Identifiants des clients de flux.
    pub(crate) next_client_id: ClientIds,
}

/// Construit le routeur (uniquement des routes `GET`, ADR 0007).
pub(crate) fn router(state: ServerState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/stream.mjpg", get(mjpeg::stream))
        .route("/health", get(health::health))
        .with_state(state)
}

/// Handler `GET /` : page HTML affichant le flux.
async fn index() -> impl IntoResponse {
    ([(header::CACHE_CONTROL, "no-cache")], Html(INDEX_HTML))
}
