//! Superviseur de capture : sonde de démarrage, capture à la demande, boucle de
//! lecture, reconnexion avec attente exponentielle (architecture § 7, ADR 0006).
//!
//! Le superviseur s'exécute dans un thread OS dédié (`camstream-capture`) car
//! `FrameSource::next_frame` est bloquant. Ses attentes (premier abonné, attente
//! exponentielle) passent par `tokio::runtime::Handle::block_on` sur un `select!`
//! entre le jeton d'arrêt, la demande et une minuterie : aucune attente active, et
//! l'arrêt reste immédiat.

use std::any::Any;
use std::num::NonZeroU32;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::hub::{DemandReceiver, FrameHub};
use super::status::{SourceStatus, StatusReceiver};
use crate::domain::{CaptureError, FrameSource, FrameSourceFactory, Recovery};

/// Nom du thread de capture (visible dans `top -H`, les logs et les paniques).
pub const CAPTURE_THREAD_NAME: &str = "camstream-capture";

/// Politique d'attente exponentielle entre deux tentatives de reconnexion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackoffPolicy {
    initial: Duration,
    max: Duration,
}

impl BackoffPolicy {
    /// Politique par défaut : 1 s, doublée à chaque échec, plafonnée à 30 s.
    pub const DEFAULT: Self = Self {
        initial: Duration::from_secs(1),
        max: Duration::from_secs(30),
    };

    /// Construit une politique ; `max` est relevé à `initial` s'il lui est inférieur.
    pub fn new(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            max: max.max(initial),
        }
    }

    /// Première attente.
    pub fn initial(&self) -> Duration {
        self.initial
    }

    /// Attente maximale.
    pub fn max(&self) -> Duration {
        self.max
    }

    /// Délai avant la tentative numéro `attempt` (1, 2, 4… fois `initial`, plafonné
    /// à `max`) ; `attempt` 0 ou 1 donnent `initial`.
    pub fn delay(&self, attempt: u32) -> Duration {
        let exponent = attempt.saturating_sub(1).min(31);
        self.initial.saturating_mul(1 << exponent).min(self.max)
    }
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Réglages du superviseur, regroupés pour garder un constructeur lisible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupervisorSettings {
    /// Attente entre deux tentatives de reconnexion.
    pub backoff: BackoffPolicy,
    /// Délai de grâce avant d'éteindre la webcam après le départ du dernier client
    /// (`Duration::ZERO` : arrêt immédiat).
    pub idle_timeout: Duration,
    /// Nombre de trames invalides **consécutives** au-delà duquel la source est
    /// considérée défaillante et rouverte (`CaptureError::CorruptedStream`). Le
    /// compteur repart de zéro à chaque trame valide.
    pub max_corrupted_frames: NonZeroU32,
}

impl SupervisorSettings {
    /// Seuil par défaut de trames invalides consécutives : 30 (≈ 2 s à 15 i/s, l'ordre
    /// de grandeur du délai de lecture par défaut).
    // Invariant vérifié à la compilation : la constante 30 est non nulle.
    pub const DEFAULT_MAX_CORRUPTED_FRAMES: NonZeroU32 = match NonZeroU32::new(30) {
        Some(max) => max,
        None => panic!("30 is not zero"),
    };
}

impl Default for SupervisorSettings {
    /// Attente exponentielle 1 s → 30 s, délai de grâce de 10 s (ADR 0006), 30 trames
    /// invalides consécutives au plus.
    fn default() -> Self {
        Self {
            backoff: BackoffPolicy::DEFAULT,
            idle_timeout: Duration::from_secs(10),
            max_corrupted_frames: Self::DEFAULT_MAX_CORRUPTED_FRAMES,
        }
    }
}

/// Superviseur de capture : capture à la demande, boucle de lecture, reconnexion.
///
/// S'exécute dans un **thread OS dédié** car `FrameSource::next_frame` est bloquant.
/// Dépend uniquement des ports `FrameSourceFactory` / `FrameSource` (inversion de
/// dépendances, pattern **Strategy** côté source et **Factory** côté création) : il
/// ignore tout de V4L2 et du transport HTTP. Ses états sont dynamiques : un `enum`
/// ([`SourceStatus`]) et des `match` les représentent (pas de Typestate ici).
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
    /// La demande est obtenue par `hub.demand()`. Le `runtime` doit être un runtime
    /// **multi-thread** : sur un runtime `current_thread`, `Handle::block_on` ne fait
    /// pas avancer les minuteries. `cancel` est normalement un jeton enfant : le
    /// superviseur ne peut pas arrêter l'application, il publie `Failed`.
    pub fn new(
        factory: Box<dyn FrameSourceFactory>,
        hub: FrameHub,
        settings: SupervisorSettings,
        runtime: tokio::runtime::Handle,
        cancel: CancellationToken,
    ) -> Self {
        let demand = hub.demand();
        let (status, _) = watch::channel(SourceStatus::Starting);
        Self {
            factory,
            hub,
            demand,
            status,
            settings,
            runtime,
            cancel,
        }
    }

    /// Récepteur de l'état de la source (pour `/health`, les logs et `main`).
    ///
    /// Le canal se ferme quand le thread de capture se termine : un récepteur qui
    /// voit la fermeture sans avoir vu `Failed` ni `Stopped` constate une fin anormale.
    pub fn status(&self) -> StatusReceiver {
        self.status.subscribe()
    }

    /// Démarre la boucle dans le thread `camstream-capture`.
    ///
    /// Si le thread panique, un gardien publie `Failed` avant la fin du thread, de
    /// sorte que `/health` et `main` voient l'échec ; [`CaptureHandle::join`] renvoie
    /// alors [`CaptureThreadError::Panicked`].
    pub fn spawn(self) -> std::io::Result<CaptureHandle> {
        let guard = PanicGuard {
            status: self.status.clone(),
        };
        let thread = std::thread::Builder::new()
            .name(CAPTURE_THREAD_NAME.to_owned())
            .spawn(move || {
                // Détruit en dernier, y compris pendant le déroulement d'une panique.
                let _guard = guard;
                self.run()
            })?;
        Ok(CaptureHandle { thread })
    }

    /// Corps du thread : supervise puis publie l'état final (`Stopped` ou `Failed`).
    fn run(mut self) -> Result<(), CaptureError> {
        let result = self.supervise();
        match &result {
            Ok(()) => {
                tracing::info!("capture stopped");
                self.publish(SourceStatus::Stopped);
            }
            Err(error) => {
                let reason = error_chain(error);
                tracing::error!(error = %reason, "fatal capture error");
                self.publish(SourceStatus::Failed { reason });
            }
        }
        result
    }

    /// Boucle principale ; `Ok` sur annulation, `Err` sur erreur fatale.
    fn supervise(&mut self) -> Result<(), CaptureError> {
        // Nombre d'échecs d'ouverture consécutifs ; 0 = aucun échec en cours.
        let mut attempt = 0;
        let mut pending = self.probe(&mut attempt)?;
        loop {
            if self.cancel.is_cancelled() {
                return Ok(());
            }
            let source = match pending.take() {
                Some(source) => source,
                None => match self.acquire(&mut attempt)? {
                    Some(source) => source,
                    None => return Ok(()),
                },
            };
            match self.stream(source, &mut attempt) {
                StreamEnd::Idle => attempt = 0,
                StreamEnd::Lost(error) => self.on_unavailable(&error, &mut attempt),
                StreamEnd::Cancelled => return Ok(()),
                StreamEnd::Fatal(error) => return Err(error),
            }
        }
    }

    /// Sonde de démarrage (ADR 0006, décision 3) : révèle immédiatement les erreurs
    /// fatales (droits, format).
    ///
    /// Si des abonnés sont **déjà** présents à l'issue de la sonde (client arrivé
    /// pendant l'ouverture), la source est conservée et sert directement : la fermer
    /// pour la rouvrir aussitôt ne ferait qu'ajouter la latence d'ouverture.
    fn probe(&mut self, attempt: &mut u32) -> Result<Option<Box<dyn FrameSource>>, CaptureError> {
        self.publish(SourceStatus::Starting);
        let started = Instant::now();
        match self.factory.create() {
            Ok(source) => {
                let description = source.description();
                let elapsed_ms = started.elapsed().as_millis();
                if self.demand.subscriber_count() > 0 {
                    tracing::info!(source = %description, elapsed_ms,
                        "startup probe succeeded, keeping the camera open for waiting viewers");
                    Ok(Some(source))
                } else {
                    tracing::info!(source = %description, elapsed_ms,
                        "startup probe succeeded, camera off until the first viewer");
                    drop(source);
                    Ok(None)
                }
            }
            Err(error) if error.recovery() == Recovery::Fatal => Err(error),
            Err(error) => {
                if self.demand.subscriber_count() == 0 {
                    tracing::warn!(error = %error_chain(&error),
                        "startup probe failed, the camera will be retried when a viewer connects");
                } else {
                    // Des clients attendent déjà : reconnexion immédiate (avec attente).
                    self.on_unavailable(&error, attempt);
                }
                Ok(None)
            }
        }
    }

    /// Obtient une source : attend la demande (si aucun échec en cours) ou l'échéance
    /// de l'attente exponentielle, puis appelle la fabrique.
    ///
    /// `Ok(None)` : arrêt demandé pendant l'attente.
    fn acquire(&mut self, attempt: &mut u32) -> Result<Option<Box<dyn FrameSource>>, CaptureError> {
        loop {
            if *attempt == 0 {
                if self.demand.subscriber_count() == 0 {
                    self.publish(SourceStatus::Idle);
                    if !self.wait_for_demand() {
                        return Ok(None);
                    }
                }
                // Depuis `Idle`, la première tentative est immédiate.
                self.publish(SourceStatus::Starting);
            } else {
                match self.wait_backoff(self.settings.backoff.delay(*attempt)) {
                    BackoffWait::Elapsed => {}
                    BackoffWait::NoDemand => {
                        tracing::info!("no viewer left, reconnection abandoned until the next one");
                        *attempt = 0;
                        continue;
                    }
                    BackoffWait::Cancelled => return Ok(None),
                }
            }
            if self.cancel.is_cancelled() {
                return Ok(None);
            }
            let started = Instant::now();
            match self.factory.create() {
                Ok(source) => {
                    tracing::info!(source = %source.description(),
                        elapsed_ms = started.elapsed().as_millis(), "camera opened");
                    return Ok(Some(source));
                }
                Err(error) if error.recovery() == Recovery::Fatal => return Err(error),
                Err(error) => self.on_unavailable(&error, attempt),
            }
        }
    }

    /// Prend acte d'une source indisponible (ouverture ratée ou perte en cours de
    /// lecture) : `Reconnecting` si des clients attendent, sinon retour à `Idle`.
    fn on_unavailable(&mut self, error: &CaptureError, attempt: &mut u32) {
        let last_error = error_chain(error);
        if self.demand.subscriber_count() == 0 {
            tracing::warn!(error = %last_error, "camera unavailable, no viewer: not retrying");
            *attempt = 0;
            return;
        }
        *attempt = attempt.saturating_add(1);
        tracing::warn!(attempt = *attempt, error = %last_error,
            retry_in_ms = self.settings.backoff.delay(*attempt).as_millis(),
            "camera unavailable, reconnecting");
        self.publish(SourceStatus::Reconnecting {
            attempt: *attempt,
            last_error,
        });
    }

    /// Boucle de lecture d'une source ouverte, jusqu'à l'échéance de grâce, une
    /// erreur ou l'arrêt. La source est détruite (RAII) avant de rendre la main.
    fn stream(&mut self, mut source: Box<dyn FrameSource>, attempt: &mut u32) -> StreamEnd {
        let description = source.description();
        let opened_at = Instant::now();
        let mut streaming = false;
        let mut frames: u64 = 0;
        // Trames invalides depuis la dernière trame valide : une source qui ne produit
        // plus que des trames corrompues est rouverte au lieu d'être lue indéfiniment.
        let mut corrupted: u32 = 0;
        let max_corrupted = self.settings.max_corrupted_frames.get();
        // Échéance du délai de grâce, armée quand la demande tombe à zéro. Vérifiée
        // entre deux trames, donc avec une précision ≤ délai de lecture.
        let mut grace_deadline: Option<Instant> = None;
        let end = loop {
            if self.cancel.is_cancelled() {
                break StreamEnd::Cancelled;
            }
            if self.demand.subscriber_count() == 0 {
                let now = Instant::now();
                let idle_timeout = self.settings.idle_timeout;
                let deadline = *grace_deadline.get_or_insert_with(|| {
                    tracing::debug!(
                        grace_ms = idle_timeout.as_millis(),
                        "last viewer left, grace period started"
                    );
                    now + idle_timeout
                });
                if now >= deadline {
                    tracing::info!(frames, "no viewer after the grace period, camera off");
                    break StreamEnd::Idle;
                }
            } else if grace_deadline.take().is_some() {
                tracing::debug!("viewer back during the grace period, camera kept on");
            }

            match source.next_frame() {
                Ok(frame) => {
                    self.hub.publish(frame);
                    frames += 1;
                    corrupted = 0;
                    *attempt = 0;
                    if !streaming {
                        streaming = true;
                        tracing::info!(source = %description,
                            first_frame_ms = opened_at.elapsed().as_millis(), "camera streaming");
                        self.publish(SourceStatus::Streaming {
                            source: description.clone(),
                        });
                    }
                }
                Err(error) => match error.recovery() {
                    Recovery::Skip => {
                        corrupted = corrupted.saturating_add(1);
                        tracing::debug!(error = %error, consecutive = corrupted, "frame skipped");
                        if corrupted >= max_corrupted {
                            break StreamEnd::Lost(CaptureError::CorruptedStream {
                                consecutive: corrupted,
                            });
                        }
                    }
                    Recovery::Reopen => break StreamEnd::Lost(error),
                    Recovery::Fatal => break StreamEnd::Fatal(error),
                },
            }
        };
        // Libération explicite (STREAMOFF, munmap, close) avant toute nouvelle tentative.
        let closing = Instant::now();
        drop(source);
        tracing::debug!(
            elapsed_ms = closing.elapsed().as_millis(),
            "camera released"
        );
        end
    }

    /// Attend le premier abonné ; `false` si l'arrêt est demandé avant.
    fn wait_for_demand(&self) -> bool {
        let mut demand = self.demand.clone();
        let cancel = self.cancel.clone();
        self.runtime.block_on(async move {
            tokio::select! {
                biased;
                () = cancel.cancelled() => false,
                () = demand.wait_for_subscribers() => true,
            }
        })
    }

    /// Attend `delay` avant la prochaine tentative, interrompu par l'arrêt ou par le
    /// départ de tous les abonnés.
    fn wait_backoff(&self, delay: Duration) -> BackoffWait {
        let mut demand = self.demand.clone();
        let cancel = self.cancel.clone();
        self.runtime.block_on(async move {
            tokio::select! {
                biased;
                () = cancel.cancelled() => BackoffWait::Cancelled,
                () = demand.wait_for_no_subscribers() => BackoffWait::NoDemand,
                () = tokio::time::sleep(delay) => BackoffWait::Elapsed,
            }
        })
    }

    /// Publie un état s'il diffère du précédent.
    ///
    /// `send_if_modified` mémorise la valeur même sans récepteur (contrairement à
    /// `send`), de sorte que `/health` lit toujours l'état courant.
    fn publish(&self, status: SourceStatus) {
        self.status.send_if_modified(|current| {
            if *current == status {
                return false;
            }
            tracing::debug!(from = ?current, to = ?status, "capture status changed");
            *current = status;
            true
        });
    }
}

/// Issue d'une boucle de lecture.
enum StreamEnd {
    /// Délai de grâce écoulé sans abonné : webcam éteinte.
    Idle,
    /// Source perdue (erreur `Reopen`, dont `Timeout`).
    Lost(CaptureError),
    /// Arrêt demandé.
    Cancelled,
    /// Erreur non récupérable.
    Fatal(CaptureError),
}

/// Issue d'une attente exponentielle.
enum BackoffWait {
    /// Délai écoulé : nouvelle tentative.
    Elapsed,
    /// Plus aucun abonné : retour à `Idle`.
    NoDemand,
    /// Arrêt demandé.
    Cancelled,
}

/// Gardien **RAII** du thread de capture : publie `Failed` si le thread se termine
/// par une panique (décision : une panique de la capture est une fin anormale qui
/// arrête l'application, au même titre qu'une erreur fatale).
struct PanicGuard {
    status: watch::Sender<SourceStatus>,
}

impl Drop for PanicGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            tracing::error!("capture thread panicked");
            self.status.send_replace(SourceStatus::Failed {
                reason: "capture thread panicked".to_owned(),
            });
        }
    }
}

/// Poignée du thread de capture.
#[derive(Debug)]
pub struct CaptureHandle {
    thread: JoinHandle<Result<(), CaptureError>>,
}

impl CaptureHandle {
    /// Vrai si le thread est terminé (`join` ne bloquera pas).
    pub fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }

    /// Attend la fin du thread (à appeler après annulation du jeton).
    ///
    /// **Bloquant et sans délai maximal** : depuis du code async, l'appeler via
    /// `tokio::task::spawn_blocking` et borner l'attente par `tokio::time::timeout`.
    /// Renvoie l'erreur fatale ou la panique ayant provoqué l'arrêt de la capture.
    pub fn join(self) -> Result<(), CaptureThreadError> {
        match self.thread.join() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(CaptureThreadError::Fatal(error)),
            Err(payload) => Err(CaptureThreadError::Panicked(panic_message(&*payload))),
        }
    }
}

/// Fin anormale du thread de capture.
#[derive(Debug, thiserror::Error)]
pub enum CaptureThreadError {
    /// La capture s'est arrêtée sur une erreur non récupérable.
    #[error("capture stopped on a fatal error")]
    Fatal(#[source] CaptureError),
    /// Le thread de capture a paniqué (message de la panique).
    #[error("capture thread panicked: {0}")]
    Panicked(String),
}

/// Extrait le message d'une panique (`&str` ou `String`), sinon un texte générique.
fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_owned())
}

/// Concatène une erreur et ses causes sur une ligne (« video I/O error: … »).
pub(crate) fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_is_capped() {
        let policy = BackoffPolicy::new(Duration::from_millis(100), Duration::from_millis(1000));
        let delays: Vec<_> = (0..=6).map(|n| policy.delay(n).as_millis()).collect();
        assert_eq!(delays, [100, 100, 200, 400, 800, 1000, 1000]);
        assert_eq!(policy.delay(u32::MAX), Duration::from_millis(1000));
    }

    #[test]
    fn backoff_max_is_never_below_initial() {
        let policy = BackoffPolicy::new(Duration::from_secs(5), Duration::from_secs(1));
        assert_eq!(policy.max(), Duration::from_secs(5));
        assert_eq!(policy.delay(3), Duration::from_secs(5));
    }

    #[test]
    fn default_settings_match_the_documentation() {
        let settings = SupervisorSettings::default();
        assert_eq!(settings.max_corrupted_frames.get(), 30);
        assert_eq!(settings.idle_timeout, Duration::from_secs(10));
        assert_eq!(settings.backoff.initial(), Duration::from_secs(1));
        assert_eq!(settings.backoff.max(), Duration::from_secs(30));
    }

    #[test]
    fn error_chain_includes_sources() {
        let error = CaptureError::Io(std::io::Error::other("ioctl failed"));
        assert_eq!(error_chain(&error), "video I/O error: ioctl failed");
    }

    #[test]
    fn panic_messages_are_extracted() {
        let caught = std::panic::catch_unwind(|| panic!("boom {}", 42)).unwrap_err();
        assert_eq!(panic_message(&*caught), "boom 42");
        let caught = std::panic::catch_unwind(|| panic!("static")).unwrap_err();
        assert_eq!(panic_message(&*caught), "static");
    }
}
