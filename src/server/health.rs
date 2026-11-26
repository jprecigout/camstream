//! Santé de l'application : DTO JSON de `/health`, handler, et client minimal
//! utilisé par la sous-commande `camstream healthcheck` (HEALTHCHECK Docker).

use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::time::Duration;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use super::routes::ServerState;
use crate::domain::SourceDescription;
use crate::stream::SourceStatus;

/// Santé globale exposée dans le champ `status` de `/health` (`"ok"` ou `"failed"`).
///
/// Type énuméré plutôt qu'une chaîne : le code HTTP en est déduit sans comparer de
/// texte, et le contrat JSON reste celui documenté.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    /// Application saine (y compris `idle` et `reconnecting`).
    Ok,
    /// Capture en échec (état `Failed`).
    Failed,
}

/// Représentation JSON de `/health` (DTO de l'adaptateur HTTP).
///
/// Implémente le pattern **DTO** : découple le contrat JSON des types du cœur, qui
/// restent sans `serde`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HealthReport {
    /// Santé globale, sérialisée en `"ok"` ou `"failed"`.
    pub status: HealthState,
    /// État de la caméra.
    pub camera: CameraReport,
    /// Clients de flux connectés.
    pub clients: usize,
}

impl HealthReport {
    /// Construit le rapport à partir de l'état de la capture et du nombre de clients.
    pub fn new(status: &SourceStatus, clients: usize) -> Self {
        Self {
            status: if status.is_failed() {
                HealthState::Failed
            } else {
                HealthState::Ok
            },
            camera: CameraReport::from(status),
            clients,
        }
    }

    /// Code HTTP associé : `503` si l'état est `failed`, `200` sinon (y compris
    /// `idle` et `reconnecting`, qui sont des états sains).
    pub fn http_status(&self) -> StatusCode {
        match self.status {
            HealthState::Failed => StatusCode::SERVICE_UNAVAILABLE,
            HealthState::Ok => StatusCode::OK,
        }
    }
}

/// État de la caméra tel qu'exposé en JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CameraReport {
    /// Nom d'état stable en `snake_case` : `starting`, `idle`, `streaming`,
    /// `reconnecting`, `failed`, `stopped`.
    pub state: &'static str,
    /// Paramètres négociés, présents en `streaming`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceReport>,
    /// Numéro de la tentative de reconnexion, présent en `reconnecting`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    /// Dernière erreur, présente en `reconnecting` et `failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Correspondance explicite cœur → JSON : renommer une variante du cœur ne change
/// pas le contrat JSON sans modification visible ici.
impl From<&SourceStatus> for CameraReport {
    fn from(status: &SourceStatus) -> Self {
        let report = |state| Self {
            state,
            source: None,
            attempt: None,
            error: None,
        };
        match status {
            SourceStatus::Starting => report("starting"),
            SourceStatus::Idle => report("idle"),
            SourceStatus::Streaming { source } => Self {
                source: Some(SourceReport::from(source)),
                ..report("streaming")
            },
            SourceStatus::Reconnecting {
                attempt,
                last_error,
            } => Self {
                attempt: Some(*attempt),
                error: Some(last_error.clone()),
                ..report("reconnecting")
            },
            SourceStatus::Failed { reason } => Self {
                error: Some(reason.clone()),
                ..report("failed")
            },
            SourceStatus::Stopped => report("stopped"),
        }
    }
}

/// DTO de `SourceDescription` (nom, format, `"640x480"`, cadence).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceReport {
    /// Nom de la source (carte de la webcam ou mire).
    pub name: String,
    /// Format natif (`MJPEG`).
    pub pixel_format: &'static str,
    /// Résolution effective (`LARGEURxHAUTEUR`).
    pub resolution: String,
    /// Cadence effective.
    pub fps: u32,
}

impl From<&SourceDescription> for SourceReport {
    fn from(description: &SourceDescription) -> Self {
        Self {
            name: description.name.clone(),
            pixel_format: description.pixel_format.as_str(),
            resolution: description.resolution.to_string(),
            fps: description.fps.get(),
        }
    }
}

/// Handler `GET /health`.
pub(crate) async fn health(State(state): State<ServerState>) -> Response {
    // Le verrou de lecture du `watch` est relâché à la fin de l'expression.
    let report = HealthReport::new(&state.status.borrow(), state.limiter.active());
    (
        report.http_status(),
        [(header::CACHE_CONTROL, "no-store")],
        Json(report),
    )
        .into_response()
}

/// Échec de la sonde `camstream healthcheck`.
#[derive(Debug, thiserror::Error)]
pub enum HealthcheckError {
    /// Connexion impossible (application arrêtée, mauvais port).
    #[error("cannot connect to {addr}")]
    Connect {
        /// Adresse interrogée.
        addr: SocketAddr,
        /// Erreur d'origine.
        #[source]
        source: std::io::Error,
    },
    /// Erreur d'entrée/sortie pendant l'échange (délai dépassé, connexion coupée).
    #[error("health request failed")]
    Io(#[source] std::io::Error),
    /// Réponse qui n'est pas une ligne de statut HTTP valide.
    #[error("invalid HTTP response: {0:?}")]
    InvalidResponse(String),
    /// L'application répond mais se déclare en échec (code HTTP non 200).
    #[error("unhealthy: HTTP {0}")]
    Unhealthy(u16),
}

/// Adresse à interroger pour une application écoutant sur `bind:port` : la boucle
/// locale de la même famille si `bind` est une adresse « toutes interfaces »
/// (`0.0.0.0`, `::`), sinon `bind` lui-même.
pub fn healthcheck_target(bind: IpAddr, port: u16) -> SocketAddr {
    let ip = match bind {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, port)
}

/// Interroge `GET /health` avec la bibliothèque standard (pas de `curl` dans l'image)
/// et réussit si le code HTTP est 200. Bloquant, chaque étape bornée par `timeout`.
pub fn check(addr: SocketAddr, timeout: Duration) -> Result<(), HealthcheckError> {
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|source| HealthcheckError::Connect { addr, source })?;
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.set_write_timeout(Some(timeout)))
        .map_err(HealthcheckError::Io)?;
    write!(
        stream,
        "GET /health HTTP/1.1\r\nHost: {addr}\r\nUser-Agent: camstream-healthcheck\r\nConnection: close\r\n\r\n"
    )
    .map_err(HealthcheckError::Io)?;

    // Seule la ligne de statut compte ; lecture bornée à 1 Kio.
    let mut status_line = String::new();
    BufReader::new(std::io::Read::take(&mut stream, 1024))
        .read_line(&mut status_line)
        .map_err(HealthcheckError::Io)?;
    let code = parse_status_code(&status_line)
        .ok_or_else(|| HealthcheckError::InvalidResponse(status_line.trim_end().to_owned()))?;
    if code == 200 {
        Ok(())
    } else {
        Err(HealthcheckError::Unhealthy(code))
    }
}

/// Extrait le code d'une ligne de statut (`HTTP/1.1 200 OK` → 200).
fn parse_status_code(line: &str) -> Option<u16> {
    let mut parts = line.split_whitespace();
    parts
        .next()
        .filter(|version| version.starts_with("HTTP/"))?;
    parts.next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Fps, PixelFormat, Resolution};

    fn json(status: &SourceStatus, clients: usize) -> String {
        serde_json::to_string(&HealthReport::new(status, clients)).unwrap()
    }

    #[test]
    fn idle_report_matches_the_documented_contract() {
        assert_eq!(
            json(&SourceStatus::Idle, 0),
            r#"{"status":"ok","camera":{"state":"idle"},"clients":0}"#
        );
        assert_eq!(
            HealthReport::new(&SourceStatus::Idle, 0).http_status(),
            StatusCode::OK
        );
    }

    #[test]
    fn streaming_report_describes_the_source() {
        let status = SourceStatus::Streaming {
            source: SourceDescription {
                name: "Cam".into(),
                pixel_format: PixelFormat::Mjpeg,
                resolution: Resolution::new(640, 480).unwrap(),
                fps: Fps::new(15).unwrap(),
            },
        };
        assert_eq!(
            json(&status, 2),
            r#"{"status":"ok","camera":{"state":"streaming","source":{"name":"Cam","pixel_format":"MJPEG","resolution":"640x480","fps":15}},"clients":2}"#
        );
    }

    #[test]
    fn reconnecting_is_healthy_and_failed_is_not() {
        let reconnecting = SourceStatus::Reconnecting {
            attempt: 3,
            last_error: "video device disconnected".into(),
        };
        assert_eq!(
            json(&reconnecting, 1),
            r#"{"status":"ok","camera":{"state":"reconnecting","attempt":3,"error":"video device disconnected"},"clients":1}"#
        );
        let failed = SourceStatus::Failed {
            reason: "permission denied".into(),
        };
        let report = HealthReport::new(&failed, 0);
        assert_eq!(report.status, HealthState::Failed);
        assert_eq!(report.http_status(), StatusCode::SERVICE_UNAVAILABLE);
        for (status, state) in [
            (SourceStatus::Starting, "starting"),
            (SourceStatus::Stopped, "stopped"),
        ] {
            assert_eq!(CameraReport::from(&status).state, state);
        }
    }

    #[test]
    fn healthcheck_targets_loopback_for_wildcard_binds() {
        let target = |ip: &str| healthcheck_target(ip.parse().unwrap(), 8080).to_string();
        assert_eq!(target("0.0.0.0"), "127.0.0.1:8080");
        assert_eq!(target("::"), "[::1]:8080");
        assert_eq!(target("192.168.1.20"), "192.168.1.20:8080");
        assert_eq!(target("127.0.0.1"), "127.0.0.1:8080");
    }

    #[test]
    fn status_lines_are_parsed() {
        assert_eq!(parse_status_code("HTTP/1.1 200 OK\r\n"), Some(200));
        assert_eq!(
            parse_status_code("HTTP/1.1 503 Service Unavailable"),
            Some(503)
        );
        assert_eq!(parse_status_code("SSH-2.0-OpenSSH"), None);
        assert_eq!(parse_status_code(""), None);
    }

    #[test]
    fn healthcheck_fails_when_nothing_listens() {
        // Port réservé puis libéré : plus rien n'y écoute.
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let error = check(addr, Duration::from_secs(1)).unwrap_err();
        assert!(matches!(error, HealthcheckError::Connect { .. }), "{error}");
    }
}
