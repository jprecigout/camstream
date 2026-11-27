//! Routeur axum : `/` (et ses fichiers `/camstream.js`, `/camstream.css`,
//! `/favicon.svg`, `/logo.svg`), `/stream.mjpg`, `/health` ;
//! toute autre route : `404`. Les en-têtes de sécurité
//! sont ajoutés à toutes les réponses, `404` comprises.

use axum::http::header;
use axum::middleware::map_response;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;
use tokio_util::sync::CancellationToken;

use super::health;
use super::mjpeg::{self, ClientIds, ClientLimiter};
use super::security::add_security_headers;
use crate::stream::{FrameHub, StatusReceiver};

/// Page d'accueil minimale, embarquée dans le binaire.
const INDEX_HTML: &str = include_str!("index.html");
/// Script de la page (fichier séparé : la CSP interdit les scripts en ligne).
const INDEX_JS: &str = include_str!("camstream.js");
/// Style de la page (fichier séparé : la CSP interdit les styles en ligne).
const INDEX_CSS: &str = include_str!("camstream.css");
/// Icône de l'application (favicon), source unique partagée avec la documentation.
const FAVICON_SVG: &str = include_str!("../../assets/logo/camstream-icon.svg");
/// Logo avec le nom, variante pour fond sombre (sans `<style>` : la CSP l'interdirait).
const LOGO_SVG: &str = include_str!("../../assets/logo/camstream-logo-dark.svg");

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
        .route("/camstream.js", get(script))
        .route("/camstream.css", get(stylesheet))
        .route("/favicon.svg", get(favicon))
        .route("/logo.svg", get(logo))
        .route("/stream.mjpg", get(mjpeg::stream))
        .route("/health", get(health::health))
        .with_state(state)
        // Appliqué après le routage : couvre aussi le `404` et le `405` d'axum.
        .layer(map_response(add_security_headers))
}

/// Handler `GET /` : page HTML affichant le flux.
async fn index() -> impl IntoResponse {
    ([(header::CACHE_CONTROL, "no-cache")], Html(INDEX_HTML))
}

/// Handler `GET /camstream.js` (type MIME exact : requis avec `nosniff`).
async fn script() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        INDEX_JS,
    )
}

/// Handler `GET /camstream.css` (type MIME exact : requis avec `nosniff`).
async fn stylesheet() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        INDEX_CSS,
    )
}

/// Handler `GET /favicon.svg` : icône de l'onglet.
async fn favicon() -> impl IntoResponse {
    svg(FAVICON_SVG)
}

/// Handler `GET /logo.svg` : logo affiché en tête de page.
async fn logo() -> impl IntoResponse {
    svg(LOGO_SVG)
}

/// Réponse SVG statique (type MIME exact : requis avec `nosniff`). `no-cache` comme
/// les autres ressources de la page : après une mise à jour du binaire, le navigateur
/// revalide et n'affiche pas un ancien logo (quelques Ko sur un réseau local).
fn svg(body: &'static str) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
}
