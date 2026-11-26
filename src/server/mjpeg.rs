//! Flux MJPEG `multipart/x-mixed-replace` : un corps HTTP infini par client, et
//! limitation du nombre de clients simultanés (ADR 0002 et 0004).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use super::routes::ServerState;
use crate::stream::FrameSubscriber;

/// Frontière entre les parties du flux multipart.
pub const BOUNDARY: &str = "frame";

/// Type de contenu de la réponse `/stream.mjpg`.
pub const CONTENT_TYPE: &str = "multipart/x-mixed-replace; boundary=frame";

/// En-têtes anti-cache : un flux en direct ne doit jamais être mis en cache.
const NO_CACHE: &str = "no-cache, no-store, must-revalidate";

/// Délai suggéré au client refusé (`Retry-After`, en secondes).
const RETRY_AFTER_SECS: &str = "5";

/// Nombre maximal de clients de flux simultanés (**Newtype**, non nul).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaxClients(NonZeroUsize);

impl MaxClients {
    /// Limite par défaut : 10 clients.
    // Invariant vérifié à la compilation : la constante 10 est non nulle.
    pub const DEFAULT: Self = match NonZeroUsize::new(10) {
        Some(max) => Self(max),
        None => panic!("10 is not zero"),
    };

    /// Construit une limite ; `None` si elle est nulle.
    pub fn new(max: usize) -> Option<Self> {
        NonZeroUsize::new(max).map(Self)
    }

    /// Nombre de clients.
    pub fn get(self) -> usize {
        self.0.get()
    }
}

impl From<NonZeroUsize> for MaxClients {
    fn from(max: NonZeroUsize) -> Self {
        Self(max)
    }
}

impl Default for MaxClients {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Limiteur de clients de flux : `MaxClients` permis.
///
/// `try_acquire_owned` réserve une place sans attente (échec → `503`) ; le
/// `OwnedSemaphorePermit` renvoyé est stocké dans l'état du corps MJPEG et libère la
/// place à sa destruction (**RAII**), même en cas de coupure brutale.
#[derive(Debug, Clone)]
pub(crate) struct ClientLimiter {
    semaphore: Arc<Semaphore>,
    max: MaxClients,
}

impl ClientLimiter {
    /// Crée un limiteur de `max` places.
    pub(crate) fn new(max: MaxClients) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(max.get())),
            max,
        }
    }

    /// Réserve une place ; `None` si la limite est atteinte.
    pub(crate) fn try_acquire(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.semaphore).try_acquire_owned().ok()
    }

    /// Nombre de clients en cours (`max - available_permits()`), pour `/health`.
    pub(crate) fn active(&self) -> usize {
        self.max
            .get()
            .saturating_sub(self.semaphore.available_permits())
    }

    /// Limite configurée.
    pub(crate) fn max(&self) -> MaxClients {
        self.max
    }
}

/// Handler `GET /stream.mjpg`.
///
/// Ordre (ADR 0006) : (1) réserver une place, sinon `503` ; (2) s'abonner au hub —
/// la demande devient visible et réveille la capture ; (3) répondre `200` tout de
/// suite ; les parties suivent dès que la webcam produit. Le handler n'attend jamais
/// la première trame.
pub(crate) async fn stream(State(state): State<ServerState>, request: Request) -> Response {
    // Absente quand le routeur est appelé sans socket (tests `oneshot`).
    let remote = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map_or_else(
            || "unknown".to_owned(),
            |ConnectInfo(addr)| addr.to_string(),
        );
    let Some(permit) = state.limiter.try_acquire() else {
        tracing::warn!(%remote, max_clients = state.limiter.max().get(),
            "stream client refused: too many clients");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [
                (header::RETRY_AFTER, RETRY_AFTER_SECS),
                (header::CACHE_CONTROL, NO_CACHE),
            ],
            "too many stream clients\n",
        )
            .into_response();
    };
    let subscriber = state.hub.subscribe();
    let session = ClientSession {
        id: state.next_client_id.fetch_add(1, Ordering::Relaxed),
        remote,
        started: Instant::now(),
        frames_sent: 0,
        pending: None,
        subscriber,
        cancel: state.shutdown.clone(),
        _permit: permit,
    };
    tracing::info!(client_id = session.id, remote = %session.remote,
        active_clients = state.limiter.active(), "stream client connected");

    let body = Body::from_stream(futures_util::stream::unfold(session, next_chunk));
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(CONTENT_TYPE)),
            (header::CACHE_CONTROL, HeaderValue::from_static(NO_CACHE)),
            (header::PRAGMA, HeaderValue::from_static("no-cache")),
        ],
        body,
    )
        .into_response()
}

/// État du corps MJPEG d'un client ; sa destruction (fin du flux, client parti,
/// arrêt) désabonne le client et rend sa place (**RAII**).
struct ClientSession {
    id: u64,
    remote: String,
    started: Instant,
    frames_sent: u64,
    /// Octets JPEG de la partie en cours, émis juste après son en-tête.
    pending: Option<Bytes>,
    subscriber: FrameSubscriber,
    cancel: CancellationToken,
    _permit: OwnedSemaphorePermit,
}

impl Drop for ClientSession {
    fn drop(&mut self) {
        tracing::info!(
            client_id = self.id,
            remote = %self.remote,
            duration_ms = self.started.elapsed().as_millis(),
            frames_sent = self.frames_sent,
            frames_skipped = self.subscriber.skipped_frames(),
            "stream client disconnected"
        );
    }
}

/// Produit le morceau suivant du corps : en-tête de partie puis octets JPEG (deux
/// `Bytes`, l'image n'est jamais copiée). `None` termine le flux.
///
/// Attend la trame **et** le jeton d'arrêt : les clones du hub gardent l'émetteur
/// vivant, sans ce `select!` l'arrêt gracieux attendrait indéfiniment.
async fn next_chunk(
    mut session: ClientSession,
) -> Option<(Result<Bytes, Infallible>, ClientSession)> {
    if let Some(data) = session.pending.take() {
        return Some((Ok(data), session));
    }
    let frame = tokio::select! {
        biased;
        () = session.cancel.cancelled() => return None,
        frame = session.subscriber.next_frame() => frame?,
    };
    session.frames_sent += 1;
    let data = frame.data().clone();
    let header = part_header(data.len());
    session.pending = Some(data);
    Some((Ok(header), session))
}

/// En-tête d'une partie. Le `\r\n` qui suit les octets JPEG de la partie précédente
/// est placé **en tête** : selon la RFC 2046 il fait partie du délimiteur
/// (`CRLF--frame`). Celui de la toute première partie forme un préambule vide.
pub fn part_header(content_length: usize) -> Bytes {
    Bytes::from(format!(
        "\r\n--{BOUNDARY}\r\nContent-Type: image/jpeg\r\nContent-Length: {content_length}\r\n\r\n"
    ))
}

/// Compteur d'identifiants de clients (pour corréler connexion et déconnexion).
pub(crate) type ClientIds = Arc<AtomicU64>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_header_follows_rfc_2046() {
        assert_eq!(
            &part_header(1234)[..],
            b"\r\n--frame\r\nContent-Type: image/jpeg\r\nContent-Length: 1234\r\n\r\n"
        );
    }

    #[test]
    fn limiter_counts_and_releases_places() {
        let limiter = ClientLimiter::new(MaxClients::new(2).unwrap());
        let first = limiter.try_acquire().unwrap();
        let second = limiter.try_acquire().unwrap();
        assert_eq!(limiter.active(), 2);
        assert!(limiter.try_acquire().is_none());
        drop(first);
        assert_eq!(limiter.active(), 1);
        assert!(limiter.try_acquire().is_some());
        drop(second);
        assert_eq!(limiter.active(), 0);
    }

    #[test]
    fn max_clients_rejects_zero() {
        assert!(MaxClients::new(0).is_none());
        assert_eq!(MaxClients::default().get(), 10);
    }
}
