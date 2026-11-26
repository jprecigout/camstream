# Architecture de Camstream

> Statut : **conception initiale (proposée)**, avant implémentation. Les décisions sont tracées
> dans les ADR [0001](adr/0001-architecture-hexagonale-et-decoupage-en-modules.md) à
> [0007](adr/0007-securite-reseau-local-v1.md). Les signatures Rust de ce document
> sont des **propositions** : l'implémentation peut les ajuster (noms de champs privés,
> détails d'API des crates), mais toute modification d'un **port** (trait) ou d'une frontière
> entre modules passe par une revue d'architecture.

## 1. Vue d'ensemble

Camstream lit les trames JPEG d'une webcam USB via V4L2 et les rediffuse, sans ré-encodage,
à plusieurs navigateurs sous forme de flux MJPEG (`multipart/x-mixed-replace`).

Exigences principales :

| Exigence | Réponse architecturale |
|---|---|
| Pas de ré-encodage sur le Pi | MJPEG natif de la webcam relayé tel quel ([ADR 0002](adr/0002-transport-mjpeg-multipart.md)) |
| Plusieurs clients, clients lents non bloquants | `FrameHub` sur `tokio::sync::broadcast` borné, trames sautées en cas de retard ([ADR 0004](adr/0004-distribution-des-trames-et-concurrence.md)) |
| Capture V4L2 bloquante | Thread OS dédié, jamais sur le runtime tokio |
| Webcam débranchée / rebranchée | `CaptureSupervisor` + `FrameSourceFactory`, reconnexion avec attente exponentielle |
| Webcam éteinte sans spectateur | Capture à la demande : compteur d'abonnés du `FrameHub`, délai de grâce, état `Idle` ([ADR 0006](adr/0006-capture-a-la-demande.md)) |
| Usage réseau local uniquement (v1) | Pas d'authentification ni de TLS, écoute `127.0.0.1` par défaut hors conteneur ([ADR 0007](adr/0007-securite-reseau-local-v1.md)) |
| Arrêt propre sur `SIGTERM` | `CancellationToken` racine, arrêt gracieux d'axum, join du thread de capture avec délai maximal |
| Testabilité sans matériel | Ports `FrameSource` / `FrameSourceFactory`, source synthétique |
| Évolutivité (WebRTC, multi-caméras) | Nouveaux adaptateurs abonnés au `FrameHub` ; un hub par caméra |

### Vue de contexte

```mermaid
flowchart LR
    User(["Utilisateur"]) -->|"navigateur web"| Browser["Navigateur"]
    Browser -->|"HTTP GET / et /stream.mjpg"| Camstream["Camstream<br/>(conteneur Docker sur Raspberry Pi)"]
    Docker["Moteur Docker"] -->|"HEALTHCHECK /health<br/>SIGTERM à l'arrêt"| Camstream
    Camstream -->|"V4L2 ioctl + mmap"| Webcam["Webcam USB<br/>/dev/video0"]
```

## 2. Découpage en modules

Une seule crate `camstream` (bibliothèque + binaire), organisée en architecture hexagonale
([ADR 0001](adr/0001-architecture-hexagonale-et-decoupage-en-modules.md)).

```text
src/
├── main.rs              # point d'entrée : parsing config, logs, signaux, anyhow
├── lib.rs               # expose les modules (tests d'intégration)
├── app.rs               # racine de composition : App, câblage, cycle de vie
├── domain/              # CŒUR — types et ports, aucune dépendance technique
│   ├── mod.rs
│   ├── frame.rs         # Frame, Resolution, Fps, SequenceNumber, PixelFormat
│   ├── device.rs        # DevicePath, CaptureSettings, SourceKind, FormatPreference
│   └── source.rs        # ports FrameSource, FrameSourceFactory ; CaptureError, Recovery, SourceDescription
├── stream/              # CŒUR — distribution des trames et supervision
│   ├── mod.rs
│   ├── hub.rs           # FrameHub, FrameSubscriber, DemandReceiver, HubCapacity
│   ├── status.rs        # SourceStatus
│   └── supervisor.rs    # CaptureSupervisor, SupervisorSettings, BackoffPolicy, CaptureHandle
├── capture/             # ADAPTATEURS de source
│   ├── mod.rs
│   ├── factory.rs       # SourceFactory (Factory)
│   ├── synthetic.rs     # SyntheticSource (mire de test JPEG embarquée)
│   └── v4l2/            # feature Cargo « v4l2 » (par défaut)
│       ├── mod.rs
│       ├── device.rs    # V4l2Device<S> (Typestate) : Opened -> Configured
│       ├── source.rs    # V4l2Source : impl FrameSource (Adapter)
│       └── encoder.rs   # JpegEncoder (repli YUYV, phase 2, feature « yuyv »)
├── server/              # ADAPTATEUR HTTP (axum)
│   ├── mod.rs           # ServerBuilder, Server, ServerError
│   ├── routes.rs        # routeur : /, /stream.mjpg, /health
│   ├── mjpeg.rs         # corps multipart en streaming, permis de client (Semaphore, RAII)
│   ├── health.rs        # HealthReport (DTO serde) + client healthcheck (sous-commande)
│   ├── net.rs           # options de socket TCP (keepalive, TCP_USER_TIMEOUT)
│   └── index.html       # page minimale (include_str!)
└── config/              # lecture et validation de la configuration
    ├── mod.rs           # AppConfig, ConfigError
    └── cli.rs           # Cli (clap derive + env)
```

### Graphe de dépendances entre modules

Les flèches indiquent « dépend de ». Le cœur (`domain`, `stream`) ne dépend d'aucun
adaptateur ni d'aucune crate technique (V4L2, HTTP, CLI).

```mermaid
flowchart TB
    main["main.rs / app<br/>(racine de composition)"]
    config["config<br/>(clap)"]
    server["server<br/>(adaptateur HTTP, axum)"]
    capture["capture<br/>(adaptateurs de source, v4l)"]
    stream["stream<br/>(cœur : FrameHub, CaptureSupervisor)"]
    domain["domain<br/>(cœur : Frame, ports, erreurs)"]

    main --> config
    main --> server
    main --> capture
    main --> stream
    config --> domain
    server --> stream
    server --> domain
    capture --> domain
    stream --> domain

    classDef core fill:#e8f4ea,stroke:#2e7d32
    classDef adapter fill:#e3f2fd,stroke:#1565c0
    class domain,stream core
    class server,capture adapter
```

### Ports et adaptateurs

| Port (trait, dans le cœur) | Rôle | Adaptateurs |
|---|---|---|
| `FrameSource` | Fournir la prochaine trame JPEG (appel bloquant) | `V4l2Source`, `SyntheticSource`, sources de test (`ScriptedSource` dans `tests/`) |
| `FrameSourceFactory` | (Re)créer une source ; utilisé à chaque reconnexion | `SourceFactory` (choix V4L2 / synthétique), fabriques de test |
| *(côté entrant)* `FrameHub` / `FrameSubscriber` | API du cœur consommée par les transports | `server::mjpeg` aujourd'hui ; WebSocket/WebRTC demain |

`FrameHub` n'est volontairement **pas** un trait : c'est le cœur lui-même. Les transports en
dépendent directement (sens autorisé adaptateur → cœur) et les tests utilisent un vrai
`FrameHub` alimenté par une source synthétique, ce qui rend un mock inutile.

### Composants à l'exécution

```mermaid
flowchart LR
    subgraph Host["Raspberry Pi"]
        Cam["Webcam USB"]
        subgraph Container["Conteneur camstream"]
            subgraph CaptureThread["Thread OS camstream-capture"]
                Sup["CaptureSupervisor"]
                Src["V4l2Source<br/>(impl FrameSource)"]
            end
            subgraph Runtime["Runtime tokio"]
                Hub[("FrameHub<br/>broadcast borné")]
                Status[("watch SourceStatus")]
                Demand[("watch nombre d'abonnés")]
                Router["Routeur axum"]
                C1["Tâche client 1<br/>flux MJPEG"]
                C2["Tâche client N<br/>flux MJPEG"]
                Health["Handler /health"]
            end
            Token{{"CancellationToken"}}
        end
    end
    Cam -->|"mmap V4L2"| Src
    Sup -->|"next_frame()"| Src
    Sup -->|"publish(Frame)"| Hub
    Sup -->|"send_replace(status)"| Status
    Hub -->|"subscribe et drop"| Demand
    Demand -->|"premier client, plus de client"| Sup
    Hub --> C1
    Hub --> C2
    Status --> Health
    Router --> C1
    Router --> C2
    Router --> Health
    Token -.->|"annulation"| Sup
    Token -.->|"annulation"| Router
    Token -.->|"annulation"| C1
    Token -.->|"annulation"| C2
    C1 -->|"multipart JPEG"| B1["Navigateur 1"]
    C2 -->|"multipart JPEG"| B2["Navigateur N"]
```

## 3. Types et ports (signatures proposées)

### 3.1 `domain::frame` — trames et newtypes

```rust
//! Types métier décrivant une trame vidéo et ses caractéristiques.

use bytes::Bytes;
use std::num::NonZeroU32;
use std::time::Instant;

/// Résolution d'image en pixels (largeur × hauteur), toujours non nulle.
///
/// Implémente le pattern **Newtype** : impossible de confondre largeur et hauteur
/// ou de construire une résolution nulle. Analysée depuis la forme « 640x480 ».
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Resolution {
    width: NonZeroU32,
    height: NonZeroU32,
}

impl Resolution {
    /// Construit une résolution ; `None` si une dimension est nulle.
    pub fn new(width: u32, height: u32) -> Option<Self>;
    /// Largeur en pixels.
    pub fn width(&self) -> u32;
    /// Hauteur en pixels.
    pub fn height(&self) -> u32;
}

/// Analyse « LARGEURxHAUTEUR » (ex. « 1280x720 »).
impl std::str::FromStr for Resolution {
    type Err = ParseResolutionError;
    fn from_str(s: &str) -> Result<Self, Self::Err>;
}

/// Nombre d'images par seconde demandé ou négocié (**Newtype**, non nul).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fps(NonZeroU32);

/// Numéro de séquence monotone d'une trame, attribué par la source (**Newtype**).
///
/// Permet aux clients et aux logs de détecter les trames sautées.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SequenceNumber(u64);

/// Format de pixels négocié avec le périphérique.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// Images JPEG compressées par la webcam (format privilégié, sans ré-encodage).
    Mjpeg,
    /// Format brut YUV 4:2:2 (repli, nécessite un encodage JPEG logiciel).
    Yuyv,
}

/// Trame vidéo **encodée en JPEG**, prête à être diffusée.
///
/// Le contenu est un `Bytes` à comptage de références : cloner une `Frame` pour
/// N clients ne copie jamais l'image (essentiel sur Raspberry Pi).
#[derive(Debug, Clone)]
pub struct Frame {
    data: Bytes,
    resolution: Resolution,
    sequence: SequenceNumber,
    captured_at: Instant,
}

impl Frame {
    /// Construit une trame à partir de données JPEG déjà encodées.
    pub fn new(data: Bytes, resolution: Resolution, sequence: SequenceNumber) -> Self;
    /// Données JPEG (partagées, sans copie).
    pub fn data(&self) -> &Bytes;
    /// Résolution de l'image.
    pub fn resolution(&self) -> Resolution;
    /// Numéro de séquence attribué par la source.
    pub fn sequence(&self) -> SequenceNumber;
    /// Instant de capture (horloge monotone), utile pour mesurer la latence.
    pub fn captured_at(&self) -> Instant;
}
```

### 3.2 `domain::device` — paramètres de capture

```rust
//! Paramètres de capture indépendants de toute bibliothèque V4L2.

use std::path::PathBuf;
use std::time::Duration;

/// Chemin d'un périphérique vidéo (ex. `/dev/video0`) (**Newtype**).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevicePath(PathBuf);

/// Type de source à instancier par la fabrique.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// Webcam réelle via V4L2.
    V4l2,
    /// Mire de test JPEG générée sans matériel (démonstration, CI).
    Synthetic,
}

/// Préférence de format de pixels demandée par la configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FormatPreference {
    /// MJPEG si disponible, sinon YUYV (comportement par défaut). En v1, sans
    /// repli YUYV, équivaut à `Mjpeg`.
    #[default]
    Auto,
    /// MJPEG exigé : erreur si la webcam ne le propose pas.
    Mjpeg,
    /// YUYV forcé (diagnostic ; coûteux en CPU). Phase 2 : refusé en v1.
    Yuyv,
}

/// Paramètres complets nécessaires pour créer une source.
///
/// Produit par le module `config`, consommé par la fabrique de `capture` :
/// ainsi `capture` ne dépend pas de `config`.
#[derive(Debug, Clone)]
pub struct CaptureSettings {
    /// Type de source.
    pub kind: SourceKind,
    /// Périphérique à ouvrir (ignoré par la source synthétique).
    pub device: DevicePath,
    /// Résolution demandée (le pilote peut en imposer une proche).
    pub resolution: Resolution,
    /// Cadence demandée.
    pub fps: Fps,
    /// Format de pixels souhaité.
    pub format: FormatPreference,
    /// Délai maximal d'attente d'une trame avant `CaptureError::Timeout`.
    pub read_timeout: Duration,
}
```

### 3.3 `domain::source` — ports de capture et erreurs

```rust
//! Ports (traits) de capture : abstractions que le cœur utilise sans connaître V4L2.

/// Source de trames vidéo, indépendante du matériel.
///
/// Implémente le pattern **Strategy** : le superviseur de capture ne connaît que ce
/// trait, ce qui permet de remplacer la webcam par une source synthétique ou de test.
///
/// Contrat :
/// - `next_frame` est **bloquant** : il ne doit être appelé que depuis le thread de
///   capture, jamais depuis le runtime tokio ;
/// - il rend la main au plus tard après le `read_timeout` configuré ;
/// - les trames renvoyées sont **toujours encodées en JPEG** ;
/// - la libération du périphérique se fait à la destruction de la source (**RAII**).
pub trait FrameSource: Send {
    /// Lit la prochaine trame disponible (bloquant, borné par le délai de lecture).
    fn next_frame(&mut self) -> Result<Frame, CaptureError>;

    /// Décrit les paramètres effectivement négociés (format, résolution, cadence).
    fn description(&self) -> SourceDescription;
}

/// Fabrique de sources, appelée au démarrage puis à chaque reconnexion.
///
/// Implémente le pattern **Factory** (fabrique abstraite à une méthode) : le
/// superviseur peut recréer une source après une déconnexion sans connaître son type.
pub trait FrameSourceFactory: Send {
    /// Crée et démarre une nouvelle source prête à produire des trames.
    fn create(&self) -> Result<Box<dyn FrameSource>, CaptureError>;
}

/// Erreurs de capture, typées pour permettre une politique de reprise.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// Le périphérique n'existe pas (pas encore branché ou chemin erroné).
    #[error("video device not found: {0}")]
    DeviceNotFound(DevicePath),
    /// Le périphérique a disparu pendant la capture (ENODEV, câble USB retiré).
    #[error("video device disconnected")]
    Disconnected,
    /// Droits insuffisants (utilisateur hors du groupe `video`, `--device` absent).
    #[error("permission denied on video device: {0}")]
    PermissionDenied(DevicePath),
    /// Aucun format compatible avec la préférence demandée.
    #[error("no supported pixel format for preference {preference:?}")]
    UnsupportedFormat { preference: FormatPreference },
    /// Aucune trame reçue dans le délai imparti.
    #[error("timed out waiting for a frame")]
    Timeout,
    /// Trame invalide (JPEG tronqué, tampon vide) : on la jette.
    #[error("corrupted frame")]
    CorruptedFrame,
    /// Autre erreur d'entrée/sortie du pilote.
    #[error("video I/O error")]
    Io(#[from] std::io::Error),
}

/// Action de reprise associée à une erreur de capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// Ignorer et relire (trame isolée perdue).
    Skip,
    /// Fermer la source et la recréer après une attente (déconnexion).
    Reopen,
    /// Erreur non récupérable : arrêter l'application (code de sortie non nul).
    Fatal,
}

impl CaptureError {
    /// Classe l'erreur selon la politique de reprise (voir § 6).
    pub fn recovery(&self) -> Recovery;
}

/// Caractéristiques effectives d'une source démarrée.
///
/// Défini dans `domain` (et non dans `stream`) car il est renvoyé par le port
/// `FrameSource::description` : le placer dans `stream` créerait une dépendance
/// `domain` → `stream` interdite. Pas de `serde` ici : la forme JSON est un DTO de
/// `server::health` (voir § 3.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceDescription {
    /// Nom lisible (ex. carte de la webcam, « synthetic »).
    pub name: String,
    /// Format natif négocié.
    pub pixel_format: PixelFormat,
    /// Résolution effective.
    pub resolution: Resolution,
    /// Cadence effective.
    pub fps: Fps,
}
```

Remarques d'implémentation :

- `DevicePath` doit implémenter `Display` (via `Path::display`) pour les messages
  `#[error("... {0}")]` : `PathBuf` n'implémente pas `Display`.
- `#[from] std::io::Error` sur `Io` rend la conversion implicite par `?` : toute erreur
  d'entrée/sortie de `v4l` deviendrait `Io` sans passer par la traduction des `errno`
  (`ENODEV` → `Disconnected`, etc.). L'adaptateur doit donc convertir explicitement
  (fonction `map_io_error`), et `#[from]` peut être retiré.

### 3.4 `stream` — diffuseur, état et superviseur

```rust
//! Cœur de distribution : publie les trames capturées vers N abonnés et expose
//! la demande (nombre d'abonnés) au superviseur de capture.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;

/// Capacité du tampon de diffusion, en trames (**Newtype**, non nulle).
#[derive(Debug, Clone, Copy)]
pub struct HubCapacity(NonZeroUsize);

/// Diffuseur de trames vers un nombre quelconque d'abonnés.
///
/// Implémente le pattern **Observer / Publish-Subscribe** au-dessus de
/// `tokio::sync::broadcast` : l'émetteur ne bloque jamais, un abonné lent saute
/// des trames au lieu de ralentir les autres. Tient aussi le compteur d'abonnés
/// (la « demande »), publié par `watch`, qui pilote la capture à la demande.
/// Cloner le hub est peu coûteux.
#[derive(Debug, Clone)]
pub struct FrameHub {
    sender: broadcast::Sender<Frame>,
    // `Arc` car chaque abonné doit pouvoir décrémenter le compteur dans son `Drop`.
    demand: Arc<watch::Sender<usize>>,
}

impl FrameHub {
    /// Crée un hub dont le tampon conserve au plus `capacity` trames.
    pub fn new(capacity: HubCapacity) -> Self;

    /// Publie une trame (synchrone, non bloquant, appelable hors runtime).
    ///
    /// Renvoie le nombre d'abonnés ; 0 n'est pas une erreur (trame jetée).
    pub fn publish(&self, frame: Frame) -> usize;

    /// Crée un nouvel abonné qui recevra les trames publiées à partir de maintenant.
    ///
    /// Incrémente la demande (`send_modify`) : le superviseur est réveillé s'il
    /// attendait un premier client.
    pub fn subscribe(&self) -> FrameSubscriber;

    /// Nombre d'abonnés actuellement vivants (valeur du compteur de demande).
    pub fn subscriber_count(&self) -> usize;

    /// Observateur de la demande, destiné au `CaptureSupervisor`.
    pub fn demand(&self) -> DemandReceiver;
}

/// Abonné au flux de trames.
///
/// **RAII** : sa destruction désabonne le récepteur `broadcast` **et** décrémente la
/// demande ; c'est le seul chemin de décrément, donc le compteur ne peut pas dériver.
/// Volontairement non `Clone`.
#[derive(Debug)]
pub struct FrameSubscriber {
    receiver: broadcast::Receiver<Frame>,
    demand: Arc<watch::Sender<usize>>,
    skipped: u64,
}

impl FrameSubscriber {
    /// Attend la prochaine trame.
    ///
    /// En cas de retard (`Lagged`), les trames perdues sont comptées et la lecture
    /// reprend avec la suivante disponible. Renvoie `None` si le hub est fermé.
    pub async fn next_frame(&mut self) -> Option<Frame>;

    /// Nombre total de trames sautées par cet abonné (client lent).
    pub fn skipped_frames(&self) -> u64;
}

impl Drop for FrameSubscriber {
    /// Décrémente la demande (`send_modify` avec `saturating_sub`).
    fn drop(&mut self);
}

/// Observateur du nombre d'abonnés du hub (**Observer**, côté superviseur).
///
/// Indépendant du transport : tout abonné compte, qu'il vienne du flux MJPEG ou
/// d'un futur transport WebRTC.
#[derive(Debug, Clone)]
pub struct DemandReceiver {
    receiver: watch::Receiver<usize>,
}

impl DemandReceiver {
    /// Nombre d'abonnés courant (lecture non bloquante, utilisable entre deux trames).
    pub fn subscriber_count(&self) -> usize;
    /// Se résout dès qu'au moins un abonné existe (immédiatement si c'est déjà le cas).
    pub async fn wait_for_subscribers(&mut self);
    /// Se résout dès que plus aucun abonné n'existe.
    pub async fn wait_for_no_subscribers(&mut self);
}

/// État observable de la capture, publié via `tokio::sync::watch`.
///
/// Aucun `serde` : `/health` le convertit en DTO (`server::health::HealthReport`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceStatus {
    /// Sonde de démarrage ou ouverture de la source en cours.
    Starting,
    /// Aucun abonné : webcam fermée, en attente du premier client.
    Idle,
    /// Trames en cours de production (y compris pendant le délai de grâce).
    Streaming { source: SourceDescription },
    /// Source perdue alors que des clients attendent ; nouvelle tentative prévue.
    Reconnecting { attempt: u32, last_error: String },
    /// Erreur fatale : l'application va s'arrêter.
    Failed { reason: String },
    /// Arrêt demandé et terminé.
    Stopped,
}

/// Politique d'attente exponentielle entre deux tentatives de reconnexion.
#[derive(Debug, Clone, Copy)]
pub struct BackoffPolicy {
    initial: Duration,
    max: Duration,
}

impl BackoffPolicy {
    /// Délai avant la tentative numéro `attempt` (1, 2, 4… plafonné à `max`).
    pub fn delay(&self, attempt: u32) -> Duration;
}

/// Réglages du superviseur, regroupés pour garder un constructeur lisible.
#[derive(Debug, Clone, Copy)]
pub struct SupervisorSettings {
    /// Attente entre deux tentatives de reconnexion.
    pub backoff: BackoffPolicy,
    /// Délai de grâce avant d'éteindre la webcam après le départ du dernier client
    /// (`Duration::ZERO` : arrêt immédiat).
    pub idle_timeout: Duration,
}

/// Superviseur de capture : capture à la demande, boucle de lecture, reconnexion.
///
/// S'exécute dans un **thread OS dédié** car `FrameSource::next_frame` est bloquant.
/// Dépend uniquement des ports `FrameSourceFactory` / `FrameSource` (inversion de
/// dépendances) : il ignore tout de V4L2 et du transport HTTP.
pub struct CaptureSupervisor {
    factory: Box<dyn FrameSourceFactory>,
    hub: FrameHub,
    demand: DemandReceiver,
    status: watch::Sender<SourceStatus>,
    settings: SupervisorSettings,
    // Permet des attentes interruptibles (`block_on` + `select!`) depuis le thread
    // de capture, qui n'est pas un thread du runtime.
    runtime: tokio::runtime::Handle,
    cancel: CancellationToken,
}

impl CaptureSupervisor {
    /// Construit le superviseur (injection des dépendances par le constructeur).
    ///
    /// La demande est obtenue par `hub.demand()`.
    pub fn new(
        factory: Box<dyn FrameSourceFactory>,
        hub: FrameHub,
        settings: SupervisorSettings,
        runtime: tokio::runtime::Handle,
        cancel: CancellationToken,
    ) -> Self;

    /// Récepteur de l'état de la source (pour `/health`, les logs et `main`).
    pub fn status(&self) -> watch::Receiver<SourceStatus>;

    /// Démarre la boucle dans le thread `camstream-capture`.
    pub fn spawn(self) -> std::io::Result<CaptureHandle>;
}

/// Poignée du thread de capture.
pub struct CaptureHandle {
    thread: std::thread::JoinHandle<Result<(), CaptureError>>,
}

impl CaptureHandle {
    /// Attend la fin du thread (à appeler après annulation du jeton).
    ///
    /// Renvoie l'erreur fatale éventuelle ayant provoqué l'arrêt de la capture.
    pub fn join(self) -> Result<(), CaptureError>;
}
```

Boucle du superviseur (pseudo-code, détail dans le § 7) :

```text
statut = Starting
sonde : source = factory.create()
    Fatal  -> statut Failed, fin du thread avec l'erreur
    Reopen -> warn, source = aucune
    Ok     -> drop(source)                      # webcam rendue tout de suite
boucle :
    statut = Idle
    attendre(premier abonné | jeton)            # block_on + select!
    tentative = 0
    boucle d'ouverture :
        statut = Starting ; créer la source
        Ok    -> boucle de lecture (ci-dessous)
        Reopen-> tentative += 1 ; statut Reconnecting
                 attendre(backoff.delay(tentative) | plus d'abonné | jeton)
                 plus d'abonné -> retour à Idle
        Fatal -> statut Failed, fin
    boucle de lecture :
        next_frame() -> publish ; tentative = 0 ; statut Streaming
        si demande == 0 : démarrer l'échéance de grâce (si pas déjà fait)
        si demande > 0  : annuler l'échéance
        échéance dépassée -> drop(source), retour à Idle
        erreur Skip -> ignorer ; erreur Reopen -> drop(source), boucle d'ouverture
jeton annulé à tout moment -> drop(source), statut Stopped, fin
```

Pendant la lecture, la demande est lue sans attente (`subscriber_count()`) entre deux trames ;
l'échéance de grâce est donc vérifiée au rythme des trames (≤ `read_timeout`), précision
largement suffisante pour un délai de 10 s.

Remarques d'implémentation (API réelles de `tokio` / `std`) :

- `broadcast::channel(n)` panique si `n == 0` (d'où `HubCapacity`) et **arrondit `n` à la
  puissance de deux supérieure** : une capacité de 5 conserve en réalité 8 trames.
- `broadcast::Sender::send` renvoie `Err` s'il n'y a aucun abonné : `publish` fait
  `send(frame).unwrap_or(0)`. `broadcast::Receiver::recv` est sûr à l'annulation
  (*cancel-safe*), donc utilisable dans `tokio::select!`.
- `watch::Sender::send` échoue **sans mémoriser la valeur** quand aucun récepteur n'existe :
  le superviseur doit utiliser `send_replace` (ou `send_modify`) pour que `/health` lise
  toujours l'état courant ; de même, le compteur de demande est modifié par `send_modify`,
  qui s'applique sans condition.
- `Handle::block_on` panique s'il est appelé **depuis** un contexte asynchrone ; il est
  valide ici car le thread `camstream-capture` est un thread OS ordinaire. Les minuteries
  (`tokio::time::sleep`) attendues ainsi utilisent le pilote de temps du runtime, ce qui
  impose deux précautions documentées par tokio :
  - sur un runtime **`current_thread`**, `Handle::block_on` ne fait pas avancer les pilotes
    d'E/S et de temps : minuteries et réveils ne fonctionnent que si un autre thread est en
    train d'exécuter `Runtime::block_on`. `#[tokio::main]` est multi-thread, mais
    `#[tokio::test]` est `current_thread` par défaut : les tests du superviseur doivent
    utiliser `#[tokio::test(flavor = "multi_thread")]` et ne jamais bloquer le thread de test
    (pas de `CaptureHandle::join` synchrone dans le test) ;
  - si le runtime est arrêté pendant qu'une minuterie est attendue via `Handle::block_on`,
    celle-ci **panique**. Cas possible si le join borné du thread de capture expire et que
    `main` se termine : sans conséquence (le processus sort), mais à ne pas confondre avec un
    défaut dans les logs.
- `watch::Receiver::wait_for(f)` renvoie un `watch::Ref`, c'est-à-dire un **verrou de
  lecture** (non `Send`) : le relâcher immédiatement (`.map(|_| ())`), ne jamais le garder à
  travers un `.await`. Il examine la valeur courante avant d'attendre, même si l'émetteur est
  déjà détruit : un `Failed` publié juste avant la fin du thread est donc bien vu. En revanche,
  si le thread se termine **sans** publier `Failed` (panique, par exemple dans le `Drop` de
  `v4l`), `wait_for` renvoie `Err(RecvError)` : `main` doit traiter ce cas comme une fin
  anormale de la capture.
- `watch::Sender<T>` est `Clone` (vérifié dans tokio 1.54) : l'`Arc` autour du compteur de demande est
  donc facultatif ; un clone du `Sender` dans chaque `FrameSubscriber` suffit.
- `std::thread::JoinHandle::join` est **bloquant et sans délai maximal** : `CaptureHandle::join`
  ne doit pas être appelé directement depuis une tâche async. `app` l'appelle via
  `tokio::task::spawn_blocking`, enveloppé dans `tokio::time::timeout(shutdown_timeout, …)`.
  `join` renvoie aussi `Err(Box<dyn Any + Send>)` si le thread a paniqué (notamment le `Drop`
  de `v4l::io::mmap::Stream`, qui panique sur une erreur de `STREAMOFF` autre que `ENODEV`) :
  ce cas doit être traduit (variante dédiée ou erreur `anyhow` dans `app`).

### 3.5 `capture` — adaptateurs

```rust
//! Adaptateurs de source : webcam V4L2, mire synthétique et fabrique.

/// Fabrique concrète choisissant l'adaptateur selon `CaptureSettings::kind`.
///
/// Implémente le pattern **Factory** : seul point du code qui connaît les types
/// concrets de source.
pub struct SourceFactory {
    settings: CaptureSettings,
}

impl FrameSourceFactory for SourceFactory {
    /// Ouvre, configure et démarre la source demandée.
    fn create(&self) -> Result<Box<dyn FrameSource>, CaptureError>;
}

/// Source synthétique : rejoue des images JPEG embarquées à la cadence demandée.
///
/// Aucun encodage à l'exécution (images pré-encodées via `include_bytes!`) :
/// utilisable sur le Pi pour valider le déploiement sans webcam, et en CI.
pub struct SyntheticSource { /* images, cadence, séquence */ }

impl FrameSource for SyntheticSource { /* ... */ }
```

```rust
//! Adaptateur V4L2 (feature Cargo « v4l2 »), encapsulant la crate `v4l`.

/// États du périphérique (**Typestate**) : un périphérique ne peut démarrer le
/// streaming qu'après avoir été configuré, vérifié à la compilation.
pub struct Opened;
/// Périphérique configuré avec un format négocié.
pub struct Configured {
    negotiated: NegotiatedFormat,
}

/// Périphérique V4L2 dans l'état `S`.
///
/// Implémente les patterns **Adapter** (masque l'API `v4l`) et **Typestate**.
pub struct V4l2Device<S> {
    device: v4l::Device,
    path: DevicePath,
    state: S,
}

impl V4l2Device<Opened> {
    /// Ouvre le périphérique ; traduit ENOENT/EACCES en `CaptureError` typées.
    pub fn open(path: &DevicePath) -> Result<Self, CaptureError>;

    /// Négocie format, résolution et cadence (MJPEG prioritaire, YUYV en repli
    /// selon `FormatPreference`).
    pub fn configure(self, settings: &CaptureSettings)
        -> Result<V4l2Device<Configured>, CaptureError>;
}

impl V4l2Device<Configured> {
    /// Alloue les tampons mmap et démarre le streaming ; consomme le périphérique.
    pub fn start_streaming(self, read_timeout: Duration) -> Result<V4l2Source, CaptureError>;
}

/// Source V4L2 en streaming ; arrête le flux et libère les tampons à la
/// destruction (**RAII**, via les `Drop` de `v4l`).
pub struct V4l2Source { /* flux mmap, format négocié, séquence, encodeur optionnel */ }

impl FrameSource for V4l2Source {
    /// Copie le tampon mmap dans un `Bytes` (seule copie de la chaîne) puis le rend
    /// au pilote ; encode en JPEG uniquement si le format négocié est YUYV.
    fn next_frame(&mut self) -> Result<Frame, CaptureError>;
    fn description(&self) -> SourceDescription;
}
```

> Constats sur `v4l` 0.14.0 (code source vérifié), à confirmer par le spike sur matériel :
>
> - **Pas d'auto-référence** : `MmapStream::with_buffers(&dev, Type::VideoCapture, n)` clone
>   l'`Arc<Handle>` du `Device` et ne l'emprunte pas ; le paramètre de durée de vie ne concerne
>   que les tampons mmap. `V4l2Source` peut donc stocker un `MmapStream<'static>` (qui est
>   `Send`) à côté de son `Device`.
> - **Délai de lecture** : `MmapStream::set_timeout(Duration)` existe ; `next()` fait un `poll`
>   puis renvoie `io::ErrorKind::TimedOut` à l'expiration.
> - **Taille utile** : `CaptureStream::next()` renvoie le tampon mmap **entier** ; il faut le
>   tronquer à `meta.bytesused` avant la copie dans `Bytes`, sinon chaque trame contient des
>   octets parasites et pèse la taille du tampon.
> - **Reprise après délai** : `next()` remet en file (`QBUF`) le tampon précédent *avant* le
>   `poll`. Après un `TimedOut`, l'appel suivant remet en file un tampon déjà en file
>   (`EINVAL` attendu). **Décision** : `Timeout` est classé `Reopen` (voir § 6). Un délai de
>   2 s à 15 i/s signifie une webcam figée, pour laquelle recréer la source est la bonne
>   réaction ; la reconnexion est de toute façon un chemin courant avec la capture à la
>   demande. Le contournement (réimplémenter la boucle `DQBUF`/`QBUF` ou corriger `v4l`) est
>   écarté : code bas niveau ou fork à maintenir pour un cas rare.
> - **`Drop`** : le `Drop` du flux panique si `STREAMOFF` échoue avec une erreur autre que
>   `ENODEV` (voir les remarques du § 3.4 sur `join`).
> - `Device::with_path` ouvre en `O_RDWR | O_NONBLOCK` ; le format se négocie par le trait
>   `v4l::video::Capture` (`enum_formats`, `set_format`, `set_params(&Parameters::with_fps(n))`),
>   le nom de la carte par `query_caps()?.card`.

### 3.6 `server` — adaptateur HTTP

```rust
//! Adaptateur HTTP (axum) : page d'accueil, flux MJPEG et santé.

/// Nombre maximal de clients de flux simultanés (**Newtype**, non nul).
#[derive(Debug, Clone, Copy)]
pub struct MaxClients(NonZeroUsize);

/// Construit le serveur HTTP étape par étape.
///
/// Implémente le pattern **Builder** : paramètres obligatoires (adresse, hub,
/// état, jeton d'arrêt) vérifiés par `build`, paramètres optionnels avec défauts
/// (`MaxClients`, options TCP).
#[derive(Debug)]
pub struct ServerBuilder { /* champs optionnels */ }

impl ServerBuilder {
    /// Démarre la construction pour l'adresse d'écoute donnée.
    pub fn new(bind: SocketAddr) -> Self;
    /// Source des trames diffusées.
    pub fn hub(self, hub: FrameHub) -> Self;
    /// État de la capture exposé par `/health`.
    pub fn status(self, status: watch::Receiver<SourceStatus>) -> Self;
    /// Limite de clients simultanés (défaut : 10).
    pub fn max_clients(self, max: MaxClients) -> Self;
    /// Détection des clients disparus (défaut : keepalive 30 s, `TCP_USER_TIMEOUT` 30 s).
    pub fn tcp_options(self, options: TcpOptions) -> Self;
    /// Jeton déclenchant l'arrêt gracieux (fin des flux en cours).
    pub fn shutdown(self, token: CancellationToken) -> Self;
    /// Valide et construit le serveur.
    pub fn build(self) -> Result<Server, ServerError>;
}

/// Serveur HTTP prêt à démarrer.
pub struct Server { /* routeur, adresse, jeton */ }

impl Server {
    /// Routeur axum, exposé pour les tests sans socket (`tower::ServiceExt::oneshot`).
    pub fn router(&self) -> axum::Router;
    /// Écoute et sert jusqu'à l'annulation du jeton.
    pub async fn run(self) -> Result<(), ServerError>;
}

/// Limiteur de clients de flux : `MaxClients` permis.
///
/// `try_acquire_owned` réserve une place sans attente (échec → `503`) ; le
/// `OwnedSemaphorePermit` renvoyé est stocké dans l'état du corps MJPEG et libère la
/// place à sa destruction (**RAII**), même en cas de coupure brutale.
#[derive(Debug, Clone)]
struct ClientLimiter {
    semaphore: Arc<tokio::sync::Semaphore>,
    max: MaxClients,
}

impl ClientLimiter {
    /// Réserve une place ; `None` si la limite est atteinte.
    fn try_acquire(&self) -> Option<tokio::sync::OwnedSemaphorePermit>;
    /// Nombre de clients en cours (`max - available_permits()`), pour `/health`.
    fn active(&self) -> usize;
}

/// Options appliquées à chaque socket acceptée (via `socket2::SockRef`).
#[derive(Debug, Clone, Copy)]
pub struct TcpOptions {
    /// Inactivité avant la première sonde keepalive.
    pub keepalive_idle: Duration,
    /// Intervalle entre sondes keepalive.
    pub keepalive_interval: Duration,
    /// Nombre de sondes keepalive sans réponse avant fermeture (`TCP_KEEPCNT`).
    pub keepalive_retries: u32,
    /// Durée maximale de données non acquittées avant fermeture par le noyau
    /// (`TCP_USER_TIMEOUT`, Linux).
    pub user_timeout: Duration,
}

/// Représentation JSON de `/health` (DTO de l'adaptateur HTTP).
///
/// Découple le contrat JSON des types du cœur, qui restent sans `serde`.
#[derive(Debug, serde::Serialize)]
pub struct HealthReport {
    /// `"ok"` ou `"failed"`.
    pub status: &'static str,
    /// État de la caméra (`state` : `starting`, `idle`, `streaming`, `reconnecting`,
    /// `failed`, `stopped`, plus les détails utiles).
    pub camera: CameraReport,
    /// Clients de flux connectés.
    pub clients: usize,
}

/// État de la caméra tel qu'exposé en JSON.
#[derive(Debug, serde::Serialize)]
pub struct CameraReport {
    /// Nom d'état stable en `snake_case`.
    pub state: &'static str,
    /// Paramètres négociés, présents en `streaming`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceReport>,
    /// Dernière erreur, présente en `reconnecting` et `failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// DTO de `SourceDescription` (nom, format, `"640x480"`, cadence).
#[derive(Debug, serde::Serialize)]
pub struct SourceReport { /* name, pixel_format, resolution, fps */ }

/// Correspondance explicite cœur → JSON : renommer une variante du cœur ne change
/// pas le contrat JSON sans modification visible ici.
impl From<&SourceStatus> for CameraReport { /* ... */ }

/// Erreurs du serveur HTTP.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// Paramètre obligatoire absent du builder.
    #[error("missing server parameter: {0}")]
    MissingParameter(&'static str),
    /// Impossible d'écouter sur l'adresse (port occupé, droits).
    #[error("cannot bind {addr}")]
    Bind { addr: SocketAddr, #[source] source: std::io::Error },
    /// Erreur fatale pendant le service.
    #[error("HTTP server error")]
    Serve(#[source] std::io::Error),
}
```

Routes :

| Route | Réponse |
|---|---|
| `GET /` | Page HTML minimale (`<img src="/stream.mjpg">`, texte « Démarrage de la caméra… » jusqu'au premier `onload`, quelques lignes de JS pour recharger l'image si la connexion tombe) |
| `GET /stream.mjpg` | `200`, `multipart/x-mixed-replace; boundary=frame`, corps infini ; `503` si `MaxClients` atteint |
| `GET /health` | JSON `HealthReport`, ex. `{"status":"ok","camera":{"state":"idle"},"clients":0}` ; `200` dans tous les états sauf `failed` → `503` |
| autre | `404` |

Format d'une partie MJPEG (deux `Bytes` émis successivement, sans copie du JPEG) :

```text
\r\n--frame\r\n
Content-Type: image/jpeg\r\n
Content-Length: <taille>\r\n
\r\n
<octets JPEG>
```

Le `\r\n` qui suit les octets JPEG est placé **en tête** de l'en-tête de la partie suivante :
selon la RFC 2046, ce CRLF fait partie du délimiteur (`CRLF--frame`). On n'émet ainsi que deux
morceaux par trame (en-tête construit, puis le `Bytes` partagé) au lieu de trois ; le CRLF
initial de la toute première partie est un préambule vide, ignoré par les navigateurs.

Le corps est construit avec `axum::body::Body::from_stream`, qui exige un flux
`Send + 'static` dont l'erreur implémente `Into<BoxError>` : l'état du flux possède le
`FrameSubscriber`, le permis `OwnedSemaphorePermit` et un clone du `CancellationToken`
(attente via `cancelled_owned()` ou clone par itération, `cancelled()` empruntant le jeton).

Ordre dans le handler `/stream.mjpg` : (1) `try_acquire` du permis, sinon `503` ;
(2) `hub.subscribe()` — la demande devient visible et réveille le superviseur ;
(3) réponse `200` immédiate avec les en-têtes multipart ; (4) les parties suivent dès que la
webcam produit ([ADR 0006](adr/0006-capture-a-la-demande.md)). Le handler n'attend jamais la
première trame.

Options TCP : `axum::serve` accepte un `Listener` ; les options sont appliquées à chaque
connexion acceptée par `axum::serve::ListenerExt::tap_io` (vérifié dans axum 0.8.9 :
`fn tap_io<F>(self, tap_fn: F) -> TapIo<Self, F> where F: FnMut(&mut Self::Io) + Send + 'static`,
appelé sur le **listener**, pas sur `Serve`) :

```rust
use axum::serve::ListenerExt;

let listener = tokio::net::TcpListener::bind(addr).await?.tap_io(move |stream| {
    // Erreur non fatale : la connexion reste servie sans les options.
    if let Err(err) = net::apply_tcp_options(socket2::SockRef::from(&*stream), &options) {
        tracing::debug!(%err, "cannot set TCP options on accepted connection");
    }
});
axum::serve(listener, router).with_graceful_shutdown(token.cancelled_owned()).await
```

Avec `socket2` 0.6 : `SockRef::set_tcp_keepalive(&TcpKeepalive::new().with_time(..)
.with_interval(..).with_retries(..))` et `SockRef::set_tcp_user_timeout(Some(..))`.
`with_retries` et `set_tcp_user_timeout` n'existent qu'avec la feature **`all`** de `socket2`
(et, pour le second, sous Linux/Android/Fuchsia), ce qui convient à la cible. `TCP_NODELAY`
(`stream.set_nodelay(true)`) peut être appliqué au même endroit pour réduire la latence des
parties.

### 3.7 `config`

```rust
//! Configuration : arguments de ligne de commande et variables d'environnement.

/// Configuration complète et validée de l'application.
///
/// Construite une seule fois au démarrage puis découpée et injectée dans chaque
/// composant : aucun composant ne lit l'environnement lui-même.
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// Paramètres de la source vidéo.
    pub capture: CaptureSettings,
    /// Adresse d'écoute HTTP.
    pub bind: SocketAddr,
    /// Limite de clients simultanés.
    pub max_clients: NonZeroUsize,
    /// Capacité du tampon de diffusion.
    pub hub_capacity: NonZeroUsize,
    /// Première attente avant reconnexion de la webcam.
    pub reconnect_initial: Duration,
    /// Attente maximale entre deux tentatives de reconnexion.
    pub reconnect_max: Duration,
    /// Délai de grâce avant d'éteindre la webcam sans client (capture à la demande).
    pub idle_timeout: Duration,
    /// Délai maximal accordé à l'arrêt gracieux.
    pub shutdown_timeout: Duration,
}

impl AppConfig {
    /// Lit la CLI et l'environnement (`CAMSTREAM_*`), applique les défauts, valide.
    pub fn load() -> Result<Self, ConfigError>;
    /// Variante testable à partir d'arguments explicites.
    pub fn try_parse_from<I, T>(args: I) -> Result<Self, ConfigError>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone;
}

/// Erreurs de configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Argument ou variable mal formé (message de clap).
    #[error(transparent)]
    Parse(#[from] clap::Error),
    /// Combinaison de valeurs incohérente.
    #[error("invalid configuration: {0}")]
    Invalid(String),
}
```

`config` ne dépend que de `domain` (newtypes) : il ne connaît ni `FrameHub` ni axum. Comme
`domain` ne doit pas dépendre de `clap`, `SourceKind` et `FormatPreference` ne peuvent pas
dériver `clap::ValueEnum` : soit `domain` implémente `FromStr` (clap l'utilise alors comme
analyseur de valeur), soit `config` déclare ses propres enums miroirs et les convertit. À noter
aussi : `try_parse_from` lit quand même les variables `CAMSTREAM_*` du processus (feature
`env`) ; les tests de configuration doivent en tenir compte. C'est
`app` qui convertit `AppConfig` en `HubCapacity`, `SupervisorSettings`, `MaxClients`.
L'adresse d'écoute par défaut est `127.0.0.1:8080` ; l'image Docker fixe
`CAMSTREAM_BIND=0.0.0.0` ([ADR 0007](adr/0007-securite-reseau-local-v1.md)). La liste des
variables et leurs défauts figure dans [deploiement.md](deploiement.md#5-variables-denvironnement).

## 4. Diagramme de classes

```mermaid
classDiagram
    direction LR

    class FrameSource {
        <<interface>>
        +next_frame() Result~Frame~
        +description() SourceDescription
    }
    class FrameSourceFactory {
        <<interface>>
        +create() Result~BoxFrameSource~
    }
    class Frame {
        -data Bytes
        -resolution Resolution
        -sequence SequenceNumber
        -captured_at Instant
        +data() Bytes
    }
    class CaptureError {
        <<enumeration>>
        DeviceNotFound
        Disconnected
        PermissionDenied
        UnsupportedFormat
        Timeout
        CorruptedFrame
        Io
        +recovery() Recovery
    }
    class FrameHub {
        -sender broadcast_Sender
        -demand ArcWatchSender
        +new(capacity) FrameHub
        +publish(frame) usize
        +subscribe() FrameSubscriber
        +subscriber_count() usize
        +demand() DemandReceiver
    }
    class FrameSubscriber {
        -receiver broadcast_Receiver
        -demand ArcWatchSender
        -skipped u64
        +next_frame() Option~Frame~
        +skipped_frames() u64
        +drop()
    }
    class DemandReceiver {
        -receiver watch_Receiver
        +subscriber_count() usize
        +wait_for_subscribers()
        +wait_for_no_subscribers()
    }
    class CaptureSupervisor {
        -factory BoxFrameSourceFactory
        -hub FrameHub
        -demand DemandReceiver
        -settings SupervisorSettings
        -runtime Handle
        -cancel CancellationToken
        +status() watch_Receiver
        +spawn() CaptureHandle
    }
    class SupervisorSettings {
        +backoff BackoffPolicy
        +idle_timeout Duration
    }
    class SourceStatus {
        <<enumeration>>
        Starting
        Idle
        Streaming
        Reconnecting
        Failed
        Stopped
    }
    class SourceFactory {
        -settings CaptureSettings
        +create() Result~BoxFrameSource~
    }
    class V4l2Source {
        +next_frame() Result~Frame~
    }
    class SyntheticSource {
        +next_frame() Result~Frame~
    }
    class V4l2Device~S~ {
        -device v4l_Device
        -state S
    }
    class Server {
        +router() Router
        +run() Result
    }
    class ServerBuilder {
        +new(bind) ServerBuilder
        +hub(hub) ServerBuilder
        +status(rx) ServerBuilder
        +max_clients(max) ServerBuilder
        +tcp_options(options) ServerBuilder
        +shutdown(token) ServerBuilder
        +build() Result~Server~
    }
    class ClientLimiter {
        -semaphore ArcSemaphore
        +try_acquire() Option~OwnedSemaphorePermit~
        +active() usize
    }
    class HealthReport {
        <<DTO serde>>
        +status str
        +camera CameraReport
        +clients usize
    }

    FrameSource <|.. V4l2Source : implémente
    FrameSource <|.. SyntheticSource : implémente
    FrameSourceFactory <|.. SourceFactory : implémente
    SourceFactory ..> V4l2Device : crée
    V4l2Device ..> V4l2Source : start_streaming
    SourceFactory ..> SyntheticSource : crée
    CaptureSupervisor o-- FrameSourceFactory
    CaptureSupervisor --> FrameHub : publie
    CaptureSupervisor --> SourceStatus : publie
    CaptureSupervisor --> DemandReceiver : observe
    CaptureSupervisor *-- SupervisorSettings
    FrameHub ..> DemandReceiver : crée
    CaptureSupervisor ..> FrameSource : utilise
    FrameSource ..> Frame : produit
    FrameSource ..> CaptureError : échoue avec
    FrameHub ..> FrameSubscriber : crée
    FrameSubscriber ..> Frame : reçoit
    ServerBuilder ..> Server : construit
    Server --> FrameHub : s'abonne
    Server --> SourceStatus : lit
    Server *-- ClientLimiter
    Server ..> HealthReport : sérialise
    HealthReport ..> SourceStatus : From
```

## 5. Flux de données et séquences

### 5.1 Chemin d'une trame (copies minimales)

```mermaid
flowchart LR
    K["Tampon mmap du pilote<br/>(noyau)"] -->|"1 copie vers Bytes"| F["Frame<br/>(Bytes partagé)"]
    F -->|"clone Arc, 0 copie"| H[("FrameHub<br/>broadcast")]
    H -->|"clone Arc"| S1["FrameSubscriber 1"]
    H -->|"clone Arc"| S2["FrameSubscriber N"]
    S1 -->|"en-tête de partie + Bytes"| T1["Socket TCP client 1"]
    S2 -->|"en-tête de partie + Bytes"| T2["Socket TCP client N"]
```

La seule copie de l'image a lieu à la sortie du tampon mmap (qui doit être rendu au pilote
pour être réutilisé). En repli YUYV, cette étape est remplacée par l'encodage JPEG.

### 5.2 Démarrage

```mermaid
sequenceDiagram
    autonumber
    participant Main as main.rs
    participant Cfg as AppConfig
    participant App as App
    participant Sup as CaptureSupervisor
    participant Srv as Server
    Main->>Cfg: load() depuis CLI et CAMSTREAM_*
    Cfg-->>Main: AppConfig validée
    Main->>Main: init tracing et CancellationToken racine
    Main->>App: App::from_config(config, token)
    App->>App: créer FrameHub et SourceFactory
    App->>Sup: new(factory, hub, settings, Handle, token enfant)
    App->>Srv: ServerBuilder ... build()
    Main->>Sup: spawn() dans le thread camstream-capture
    Sup->>Sup: sonde, créer puis détruire la source
    alt erreur Fatal
        Sup-->>Main: statut Failed
        Main->>Main: annuler le jeton racine, sortie code 1
    else sonde réussie ou webcam absente
        Sup-->>Main: statut Idle
    end
    Main->>Srv: run() sur le runtime tokio
    Main->>Main: select sur SIGTERM, SIGINT, fin du serveur, statut Failed
```

Le `select!` final de `main` est le **seul** endroit qui décide de l'arrêt global : sur
`Failed` (observé par `status.wait_for`), il annule le jeton racine puis suit la séquence
du § 5.4 et sort en code 1 ([ADR 0004](adr/0004-distribution-des-trames-et-concurrence.md),
décision 5).

### 5.3 Connexion d'un client et streaming

```mermaid
sequenceDiagram
    autonumber
    participant B as Navigateur
    participant R as Routeur axum
    participant M as Handler MJPEG
    participant H as FrameHub
    participant S as CaptureSupervisor
    participant V as V4l2Source

    B->>R: GET /
    R-->>B: 200 page HTML avec img src=/stream.mjpg
    B->>R: GET /stream.mjpg
    R->>M: requête
    M->>M: try_acquire_owned sur le Semaphore
    alt limite MaxClients atteinte
        M-->>B: 503 Service Unavailable
    else place disponible
        M->>H: subscribe()
        H-->>M: FrameSubscriber
        M-->>B: 200 multipart/x-mixed-replace, corps en streaming
        loop chaque trame
            S->>V: next_frame() bloquant
            V-->>S: Frame JPEG
            S->>H: publish(frame) sans attente
            H-->>M: next_frame() renvoie la trame
            M-->>B: en-tête de partie puis octets JPEG
        end
        opt client lent
            H-->>M: Lagged(n) interne
            M->>M: compter n trames sautées et continuer
        end
        B-xM: fermeture de l'onglet
        M->>M: destruction du flux, du FrameSubscriber et du permis
    end
```

### 5.4 Arrêt propre (`SIGTERM`)

```mermaid
sequenceDiagram
    autonumber
    participant D as Docker
    participant Main as main.rs
    participant T as CancellationToken
    participant Srv as Server axum
    participant C as Flux MJPEG clients
    participant Sup as Thread de capture
    D->>Main: SIGTERM
    Main->>T: cancel()
    par serveur
        T-->>Srv: arrêt gracieux, plus de nouvelles connexions
        T-->>C: chaque flux se termine proprement
        Srv-->>Main: run() terminé
    and capture
        T-->>Sup: is_cancelled() vrai après la lecture en cours
        Sup->>Sup: drop de la source donc STREAMOFF et libération du périphérique
        Sup-->>Main: thread terminé
    end
    Main->>Main: join borné par shutdown_timeout
    Main-->>D: exit 0
```

Point d'attention : les clones de `FrameHub` détenus par le serveur gardent l'émetteur
`broadcast` vivant ; un flux MJPEG ne se termine donc **pas** par `Closed`. Chaque flux doit
attendre `tokio::select!` sur `subscriber.next_frame()` **et** `token.cancelled()`, sans quoi
l'arrêt gracieux d'axum attendrait indéfiniment la fin des connexions longues.

Ce `select!` ne suffit pas toujours : si l'écriture vers un client est bloquée (fenêtre TCP
pleine, client disparu sans `RST`), hyper ne sonde plus le flux du corps et le jeton n'est
jamais observé. Le futur `Server::run` doit donc lui aussi être borné par
`tokio::time::timeout(shutdown_timeout, …)` dans `app`, au même titre que le join du thread de
capture.
Les options `SO_KEEPALIVE` / `TCP_USER_TIMEOUT` (§ 3.6) réduisent ce cas en fonctionnement
normal : un client disparu est déconnecté par le noyau au bout d'environ 30 s.

### 5.5 Capture à la demande : premier client, rechargement, dernier départ

```mermaid
sequenceDiagram
    autonumber
    participant B as Navigateur
    participant M as Handler MJPEG
    participant H as FrameHub
    participant D as DemandReceiver
    participant S as CaptureSupervisor
    participant F as SourceFactory
    participant V as V4l2Source

    Note over S: statut Idle, webcam fermée
    S->>D: block_on wait_for_subscribers ou jeton
    B->>M: GET /stream.mjpg
    M->>M: permis du Semaphore
    M->>H: subscribe()
    H->>D: demande 0 vers 1
    M-->>B: 200 et en-têtes multipart, sans attendre
    D-->>S: réveil, au moins un abonné
    S->>F: create()
    F->>V: open, configure, start_streaming
    F-->>S: source prête, statut Streaming
    loop tant que la demande est non nulle
        S->>V: next_frame()
        V-->>S: Frame
        S->>H: publish(frame)
        H-->>M: trame
        M-->>B: partie JPEG
    end
    B-xM: rechargement de la page
    M->>H: drop du FrameSubscriber, demande 1 vers 0
    S->>S: demande nulle, échéance de grâce dans idle_timeout
    B->>M: nouveau GET /stream.mjpg avant l'échéance
    M->>H: subscribe(), demande 0 vers 1
    S->>S: échéance annulée, la webcam reste ouverte
    Note over B,M: première image immédiate
    B-xM: fermeture de l'onglet
    M->>H: drop, demande 1 vers 0
    S->>S: échéance atteinte sans nouvel abonné
    S->>V: drop donc STREAMOFF et fermeture
    Note over S: statut Idle
```

Latence du premier client : ouverture et première trame (≈ 0,3 à 1,5 s selon la webcam)
pendant lesquelles la connexion est ouverte mais sans image ; la page HTML affiche
« Démarrage de la caméra… » jusqu'au premier `onload` de l'image. Les spectateurs suivants,
et les rechargements dans le délai de grâce, ne subissent pas cette latence.

## 6. Gestion des erreurs

| Couche | Type | Traitement |
|---|---|---|
| `domain` / `capture` | `CaptureError` | Classée par `recovery()` puis traitée par le superviseur |
| `server` | `ServerError` | Remonte à `main` (port occupé → sortie code 1) |
| Flux client | erreurs d'écriture socket | Fin silencieuse du flux (log `debug`), aucune propagation |
| `config` | `ConfigError` | Message clair et sortie code 2 (`--help`/`--version` gérés par clap) |
| `main` / `app` | `anyhow::Error` | Contexte ajouté (`.context(...)`), log `error`, code de sortie non nul |

Politique de reprise de la capture :

| `CaptureError` | `Recovery` | Comportement |
|---|---|---|
| `CorruptedFrame` | `Skip` | Trame jetée, log `debug` |
| `Timeout` | `Reopen` | Webcam figée : la source est détruite puis recréée (choix lié au comportement de `v4l` 0.14, § 3.5) |
| `Disconnected`, `DeviceNotFound`, `Io` | `Reopen` | Statut `Reconnecting`, attente exponentielle (1 s, 2 s, 4 s… max 30 s), nouvel appel à la fabrique — **uniquement tant qu'il y a des abonnés** ; sinon `Idle` |
| `PermissionDenied`, `UnsupportedFormat` (dont « pas de MJPEG » en v1) | `Fatal` | Statut `Failed`, fin du thread de capture avec l'erreur ; `main` l'observe (statut `watch` ou fin du thread) et annule le jeton racine, sortie code 1 (la politique `restart` de Docker prend le relais) |

Le superviseur reçoit un jeton **enfant** (§ 5.2) : annuler un `child_token()` n'annule pas
son parent. Le superviseur ne peut donc pas déclencher lui-même l'arrêt global ; c'est `main`
qui réagit à `Failed`.

Aucun `unwrap()`/`expect()` hors tests ; les invariants (ex. constantes non nulles) passent
par des constructeurs `const` ou sont commentés.

## 7. Cycle de vie de la capture, capture à la demande et reconnexion

```mermaid
stateDiagram-v2
    [*] --> Starting
    Starting --> Idle : sonde réussie ou webcam absente, aucun abonné
    Starting --> Streaming : source créée et abonnés présents
    Starting --> Reconnecting : erreur Reopen avec abonnés
    Starting --> Failed : erreur Fatal

    Idle --> Starting : premier abonné

    state Streaming {
        [*] --> Active
        Active --> Grace : demande à 0
        Grace --> Active : nouvel abonné avant l'échéance
    }
    Streaming --> Idle : échéance de grâce atteinte, source détruite
    Streaming --> Reconnecting : erreur Reopen, dont Timeout
    Streaming --> Failed : erreur Fatal

    Reconnecting --> Streaming : create() réussi après attente
    Reconnecting --> Reconnecting : nouvel échec, attente doublée
    Reconnecting --> Idle : plus aucun abonné
    Reconnecting --> Failed : erreur Fatal

    Idle --> Stopped : jeton annulé
    Starting --> Stopped : jeton annulé
    Streaming --> Stopped : jeton annulé
    Reconnecting --> Stopped : jeton annulé pendant l'attente
    Failed --> [*]
    Stopped --> [*]
```

`Active` et `Grace` sont des sous-états internes du superviseur ; l'état publié reste
`Streaming` dans les deux cas (la webcam est ouverte). `Starting` couvre la sonde initiale et
chaque ouverture déclenchée par un premier abonné.

Règles :

- l'attente de reconnexion est **interruptible** par le jeton (pas de `thread::sleep` de 30 s
  qui retarderait l'arrêt). `CancellationToken` n'offre **aucune attente synchrone** : la
  solution retenue est `tokio::runtime::Handle::block_on` sur un `select!` entre
  `cancelled()`, la demande et `tokio::time::sleep` (le `Handle` est injecté au superviseur,
  [ADR 0006](adr/0006-capture-a-la-demande.md)) ;
- toutes les attentes du thread de capture (premier abonné, attente exponentielle) sont des
  `Handle::block_on` sur un `select!` (jeton, demande, minuterie) : aucune attente active ;
- pendant `Reconnecting`, les clients restent connectés : ils ne reçoivent plus de trames,
  puis le flux reprend sur la **même connexion** quand la webcam revient ;
- l'attente exponentielle est interrompue si la demande tombe à 0 (→ `Idle`, tentatives
  remises à zéro) ; depuis `Idle`, la première tentative au retour d'un abonné est immédiate ;
- sans abonné, la webcam n'est pas surveillée : un débranchement n'est détecté qu'au client
  suivant ;
- l'ancienne source est détruite (RAII) **avant** la tentative suivante, pour libérer le
  périphérique ;
- le compteur de tentatives est remis à zéro dès qu'une trame est publiée.

Limite côté Docker : la reconnexion suppose que le nœud du périphérique reste valide dans le
conteneur après rebranchement (voir [deploiement.md](deploiement.md#7-reconnexion-de-la-webcam-dans-docker)).

## 8. Concurrence

| Activité | Exécution | Communication |
|---|---|---|
| Lecture V4L2, reconnexion, capture à la demande | Thread OS `camstream-capture` | `FrameHub::publish` (synchrone), `watch::Sender::send_replace`, `DemandReceiver` ; attentes via `Handle::block_on` |
| Serveur HTTP et flux clients | Runtime tokio multi-thread (une tâche par connexion) | `FrameSubscriber::next_frame().await` |
| Signaux | Tâche tokio (`tokio::signal::unix`) | `CancellationToken::cancel()` |

- Aucun verrou partagé entre capture et clients : uniquement des canaux (`broadcast`, `watch`).
- Limite de clients : `Arc<Semaphore>` et `try_acquire_owned()` (ADR 0004, décision 6) ;
  réservation atomique par construction, libération par `Drop` du permis.
- Demande (capture à la demande) : `watch<usize>` du `FrameHub`, incrémenté dans `subscribe`,
  décrémenté dans `Drop for FrameSubscriber`, observé par le superviseur.
- Threads du runtime : `worker_threads` laissé au défaut (nombre de cœurs du Pi) ; à réduire à 2
  si les mesures montrent une contention avec la capture.

### Format YUYV de repli

Si la webcam ne propose pas MJPEG, le navigateur ne peut pas afficher du YUYV : il faut encoder
en JPEG sur le Pi. Cet encodage est confiné à `V4l2Source` (stratégie `JpegEncoder` interne,
crate `jpeg-encoder`, qualité `CAMSTREAM_JPEG_QUALITY`) afin que le contrat « trames JPEG » du
port reste vrai. Il est prévu en **phase 2** (feature `yuyv`) ; en phase 1, l'absence de MJPEG
produit `UnsupportedFormat` (erreur fatale explicite). En YUYV, limiter la résolution (640x480)
et la cadence (10 à 15 i/s).

## 9. Observabilité

- `tracing` avec `EnvFilter` (`RUST_LOG`, défaut `info`).
- Un *span* par client (`client_id`, adresse distante) : `info` à la connexion et à la
  déconnexion (durée, trames envoyées, trames sautées).
- `warn` à chaque perte de webcam et tentative de reconnexion, `info` au rétablissement.
- `info` à l'allumage et à l'extinction de la webcam (capture à la demande), avec la durée
  d'ouverture mesurée (latence du premier client).
- Aucune trace par trame au-dessus du niveau `trace`.

## 10. Évolutions prévues (non implémentées)

| Évolution | Impact |
|---|---|
| WebSocket / WebRTC | Nouvel adaptateur de transport abonné au `FrameHub` ; ADR dédié |
| Plusieurs caméras | Un couple `CaptureSupervisor` + `FrameHub` par caméra, routes `/cameras/{id}/stream.mjpg` |
| `/snapshot.jpg` | Nécessite de conserver la dernière trame (un `watch<Option<Frame>>` en plus du hub) |
| Authentification / TLS | Hors v1 ([ADR 0007](adr/0007-securite-reseau-local-v1.md)) : reverse proxy en amont ou VPN, nouvel ADR |
| Conversion YUYV → JPEG | Phase 2 (feature `yuyv`) ; en v1, webcam sans MJPEG = erreur fatale explicite |
