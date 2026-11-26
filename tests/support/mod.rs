//! Outils partagés des tests d'intégration : fabrique et source scriptées, démarrage
//! de l'application sur un port libre, client HTTP et lecteur multipart incrémental.
//!
//! Chaque fichier de `tests/` est une crate distincte qui n'utilise qu'une partie
//! de ces outils : d'où l'autorisation du code mort ci-dessous.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use camstream::app::{App, Outcome};
use camstream::config::AppConfig;
use camstream::domain::{
    CaptureError, CaptureSettings, DevicePath, FormatPreference, Fps, Frame, FrameSource,
    FrameSourceFactory, PixelFormat, Resolution, SequenceNumber, SourceDescription, SourceKind,
};
use camstream::stream::{SourceStatus, StatusReceiver};
use http_body_util::{BodyExt, Empty};
use hyper::body::Incoming;
use hyper::Response;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Délai maximal de toute attente des tests : large, pour ne jamais échouer sur une
/// machine chargée ; les tests réussis n'attendent que quelques dizaines de ms.
pub const PATIENCE: Duration = Duration::from_secs(10);

/// Plus petit JPEG « structurel » (SOI … EOI) : suffit au cœur, qui ne décode pas.
pub const TINY_JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xD9];

/// Attend un futur au plus [`PATIENCE`], avec un message explicite sinon.
pub async fn within<T>(what: &str, future: impl Future<Output = T>) -> T {
    match tokio::time::timeout(PATIENCE, future).await {
        Ok(value) => value,
        Err(_) => panic!("timed out waiting for {what}"),
    }
}

/// Attend qu'un état satisfasse `predicate` et le renvoie.
pub async fn wait_status(
    status: &mut StatusReceiver,
    what: &str,
    predicate: impl FnMut(&SourceStatus) -> bool,
) -> SourceStatus {
    within(what, async {
        status
            .wait_for(predicate)
            .await
            .map(|current| current.clone())
            .unwrap_or_else(|_| panic!("status channel closed while waiting for {what}"))
    })
    .await
}

/// Attend qu'une condition sur un compteur devienne vraie (sondage toutes les 5 ms).
pub async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    within(what, async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
}

/// Erreurs reproductibles (`CaptureError` n'est pas `Clone`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// `Disconnected` (Reopen).
    Disconnected,
    /// `Timeout` (Reopen).
    Timeout,
    /// `CorruptedFrame` (Skip).
    Corrupted,
    /// `PermissionDenied` (Fatal).
    PermissionDenied,
    /// Panique (bogue simulé, par exemple dans un `Drop` de `v4l`).
    Panic,
}

impl Failure {
    fn raise(self) -> CaptureError {
        match self {
            Self::Disconnected => CaptureError::Disconnected,
            Self::Timeout => CaptureError::Timeout,
            Self::Corrupted => CaptureError::CorruptedFrame,
            Self::PermissionDenied => {
                CaptureError::PermissionDenied(DevicePath::new("/dev/video0"))
            }
            Self::Panic => panic!("simulated capture bug"),
        }
    }
}

/// Étape scriptée de la fabrique.
#[derive(Debug, Clone, Copy)]
pub enum Step {
    /// `create()` échoue.
    Fail(Failure),
    /// `create()` réussit ; la source produit `frames` trames puis échoue avec
    /// `then` (ou produit indéfiniment si `frames` vaut `None`).
    Source {
        /// Trames produites avant l'erreur.
        frames: Option<u64>,
        /// Erreur finale.
        then: Failure,
    },
}

impl Step {
    /// Source saine qui produit indéfiniment.
    pub const HEALTHY: Self = Self::Source {
        frames: None,
        then: Failure::Disconnected,
    };
}

/// Compteurs observables d'une [`ScriptedFactory`].
#[derive(Debug, Clone, Default)]
pub struct Probe {
    creates: Arc<AtomicUsize>,
    live: Arc<AtomicUsize>,
}

impl Probe {
    /// Appels à `create()` (réussis ou non).
    pub fn creates(&self) -> usize {
        self.creates.load(Ordering::SeqCst)
    }

    /// Sources actuellement vivantes (webcam « allumée »).
    pub fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }
}

/// Fabrique scriptée : applique les étapes dans l'ordre, puis `Step::HEALTHY`.
///
/// Implémente le port `FrameSourceFactory` (pattern **Factory**) pour tester la
/// supervision sans matériel.
pub struct ScriptedFactory {
    steps: Mutex<VecDeque<Step>>,
    interval: Duration,
    probe: Probe,
}

impl ScriptedFactory {
    /// Fabrique dont les sources produisent une trame toutes les `interval`.
    pub fn new(steps: impl IntoIterator<Item = Step>, interval: Duration) -> (Box<Self>, Probe) {
        let probe = Probe::default();
        let factory = Box::new(Self {
            steps: Mutex::new(steps.into_iter().collect()),
            interval,
            probe: probe.clone(),
        });
        (factory, probe)
    }
}

impl FrameSourceFactory for ScriptedFactory {
    fn create(&self) -> Result<Box<dyn FrameSource>, CaptureError> {
        self.probe.creates.fetch_add(1, Ordering::SeqCst);
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Step::HEALTHY);
        match step {
            Step::Fail(failure) => Err(failure.raise()),
            Step::Source { frames, then } => {
                self.probe.live.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(ScriptedSource {
                    remaining: frames,
                    then,
                    interval: self.interval,
                    sequence: SequenceNumber::default(),
                    live: Arc::clone(&self.probe.live),
                }))
            }
        }
    }
}

/// Source scriptée (pattern **Strategy**) : trames minimales à intervalle fixe.
pub struct ScriptedSource {
    remaining: Option<u64>,
    then: Failure,
    interval: Duration,
    sequence: SequenceNumber,
    live: Arc<AtomicUsize>,
}

impl FrameSource for ScriptedSource {
    fn next_frame(&mut self) -> Result<Frame, CaptureError> {
        std::thread::sleep(self.interval);
        if let Some(remaining) = &mut self.remaining {
            if *remaining == 0 {
                return Err(self.then.raise());
            }
            *remaining -= 1;
        }
        let frame = Frame::new(
            Bytes::from_static(TINY_JPEG),
            Resolution::new(1, 1).unwrap(),
            self.sequence,
        );
        self.sequence = self.sequence.next();
        Ok(frame)
    }

    fn description(&self) -> SourceDescription {
        SourceDescription {
            name: "scripted".into(),
            pixel_format: PixelFormat::Mjpeg,
            resolution: Resolution::new(1, 1).unwrap(),
            fps: Fps::new(100).unwrap(),
        }
    }
}

impl Drop for ScriptedSource {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Configuration de test : mire synthétique rapide, port libre, délais courts.
pub fn test_config() -> AppConfig {
    AppConfig {
        capture: CaptureSettings {
            kind: SourceKind::Synthetic,
            device: DevicePath::new("/dev/null"),
            resolution: Resolution::new(640, 480).unwrap(),
            fps: Fps::new(50).unwrap(),
            format: FormatPreference::Auto,
            read_timeout: Duration::from_secs(1),
        },
        bind: "127.0.0.1:0".parse().unwrap(),
        max_clients: 4.try_into().unwrap(),
        hub_capacity: 4.try_into().unwrap(),
        reconnect_initial: Duration::from_millis(20),
        reconnect_max: Duration::from_millis(100),
        idle_timeout: Duration::from_millis(200),
        shutdown_timeout: Duration::from_secs(3),
    }
}

/// Application démarrée en tâche de fond.
pub struct TestApp {
    /// Adresse d'écoute effective.
    pub addr: SocketAddr,
    /// État de la capture.
    pub status: StatusReceiver,
    /// Jeton simulant `SIGTERM`.
    pub shutdown: CancellationToken,
    /// Tâche exécutant `BoundApp::run`.
    pub task: JoinHandle<anyhow::Result<Outcome>>,
}

impl TestApp {
    /// Démarre l'application (fabrique réelle si `factory` vaut `None`).
    pub async fn start(config: AppConfig, factory: Option<Box<dyn FrameSourceFactory>>) -> Self {
        let app = match factory {
            Some(factory) => App::with_factory(config, factory),
            None => App::new(config),
        };
        let bound = app.bind().await.expect("app binds");
        let addr = bound.local_addr();
        let status = bound.status();
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(bound.run(shutdown.clone().cancelled_owned()));
        Self {
            addr,
            status,
            shutdown,
            task,
        }
    }

    /// Simule `SIGTERM` et renvoie l'issue.
    pub async fn stop(self) -> Outcome {
        self.shutdown.cancel();
        self.outcome().await
    }

    /// Attend la fin de l'application et renvoie l'issue.
    pub async fn outcome(self) -> Outcome {
        within("application end", self.task)
            .await
            .expect("run task")
            .expect("run result")
    }
}

/// Envoie `GET path` sur une connexion HTTP/1.1 dédiée.
pub async fn get(addr: SocketAddr, path: &str) -> Response<Incoming> {
    request(addr, hyper::Method::GET, path).await
}

/// Envoie une requête sans corps sur une connexion HTTP/1.1 dédiée.
pub async fn request(addr: SocketAddr, method: hyper::Method, path: &str) -> Response<Incoming> {
    within("HTTP response", async {
        let stream = TcpStream::connect(addr).await.expect("connect");
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .expect("handshake");
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let request = hyper::Request::builder()
            .method(method)
            .uri(path)
            .header(hyper::header::HOST, addr.to_string())
            .body(Empty::<Bytes>::new())
            .expect("request");
        sender.send_request(request).await.expect("response")
    })
    .await
}

/// Lit un corps de réponse complet (réponses finies uniquement).
pub async fn body_text(response: Response<Incoming>) -> String {
    let bytes = within("response body", response.into_body().collect())
        .await
        .expect("body")
        .to_bytes();
    String::from_utf8(bytes.to_vec()).expect("UTF-8 body")
}

/// Partie d'un flux multipart.
#[derive(Debug)]
pub struct Part {
    /// En-têtes de la partie, noms en minuscules.
    pub headers: Vec<(String, String)>,
    /// Contenu.
    pub data: Bytes,
}

impl Part {
    /// Valeur d'un en-tête.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Lecteur incrémental d'un flux `multipart/x-mixed-replace; boundary=frame` :
/// lit le corps **morceau par morceau** (jamais en entier, il est infini).
pub struct MultipartReader {
    body: Incoming,
    buffer: BytesMut,
    /// Nombre de morceaux de corps reçus.
    pub chunks: usize,
}

impl MultipartReader {
    /// Enveloppe le corps d'une réponse `/stream.mjpg`.
    pub fn new(response: Response<Incoming>) -> Self {
        Self {
            body: response.into_body(),
            buffer: BytesMut::new(),
            chunks: 0,
        }
    }

    /// Lit la partie suivante ; `None` si le flux se termine.
    pub async fn next_part(&mut self) -> Option<Part> {
        within("next multipart part", async {
            loop {
                if let Some(part) = self.parse_part() {
                    return Some(part);
                }
                match self.body.frame().await {
                    None | Some(Err(_)) => return None,
                    Some(Ok(frame)) => {
                        if let Ok(data) = frame.into_data() {
                            self.chunks += 1;
                            self.buffer.extend_from_slice(&data);
                        }
                    }
                }
            }
        })
        .await
    }

    /// Extrait une partie complète du tampon, s'il en contient une.
    fn parse_part(&mut self) -> Option<Part> {
        const DELIMITER: &[u8] = b"--frame\r\n";
        let start = find(&self.buffer, DELIMITER)? + DELIMITER.len();
        let header_end = start + find(&self.buffer[start..], b"\r\n\r\n")?;
        let head = std::str::from_utf8(&self.buffer[start..header_end]).expect("ASCII headers");
        let headers: Vec<(String, String)> = head
            .split("\r\n")
            .filter_map(|line| line.split_once(':'))
            .map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_owned()))
            .collect();
        let length: usize = headers
            .iter()
            .find(|(key, _)| key == "content-length")
            .expect("Content-Length header")
            .1
            .parse()
            .expect("numeric Content-Length");
        let data_start = header_end + 4;
        if self.buffer.len() < data_start + length {
            return None;
        }
        let mut consumed = self.buffer.split_to(data_start + length);
        let data = consumed.split_off(data_start).freeze();
        Some(Part { headers, data })
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
