//! Diffuseur de trames : publie les trames capturées vers N abonnés et expose la
//! demande (nombre d'abonnés) au superviseur de capture (ADR 0004 et 0006).

use std::num::NonZeroUsize;

use tokio::sync::{broadcast, watch};

use crate::domain::Frame;

/// Capacité du tampon de diffusion, en trames (**Newtype**, non nulle).
///
/// `tokio::sync::broadcast` panique sur une capacité nulle et arrondit la capacité
/// à la puissance de deux supérieure (5 → 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HubCapacity(NonZeroUsize);

impl HubCapacity {
    /// Capacité par défaut : 4 trames (≈ 270 ms à 15 i/s, ADR 0004).
    // Invariant vérifié à la compilation : la constante 4 est non nulle.
    pub const DEFAULT: Self = match NonZeroUsize::new(4) {
        Some(capacity) => Self(capacity),
        None => panic!("4 is not zero"),
    };

    /// Construit une capacité ; `None` si elle est nulle.
    pub fn new(capacity: usize) -> Option<Self> {
        NonZeroUsize::new(capacity).map(Self)
    }

    /// Nombre de trames.
    pub fn get(self) -> usize {
        self.0.get()
    }
}

impl From<NonZeroUsize> for HubCapacity {
    fn from(capacity: NonZeroUsize) -> Self {
        Self(capacity)
    }
}

impl Default for HubCapacity {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Diffuseur de trames vers un nombre quelconque d'abonnés.
///
/// Implémente le pattern **Observer / Publish-Subscribe** au-dessus de
/// `tokio::sync::broadcast` : l'émetteur ne bloque jamais, un abonné lent saute
/// des trames au lieu de ralentir les autres. Tient aussi le compteur d'abonnés
/// (la « demande »), publié par `watch`, qui pilote la capture à la demande.
/// Cloner le hub est peu coûteux (canaux partagés).
#[derive(Debug, Clone)]
pub struct FrameHub {
    sender: broadcast::Sender<Frame>,
    // `watch::Sender` est `Clone` (tokio ≥ 1.37) : chaque abonné en garde un clone
    // pour décrémenter la demande dans son `Drop`, sans `Arc` supplémentaire.
    demand: watch::Sender<usize>,
}

impl FrameHub {
    /// Crée un hub dont le tampon conserve au plus `capacity` trames (arrondi à la
    /// puissance de deux supérieure par `broadcast`).
    pub fn new(capacity: HubCapacity) -> Self {
        let (sender, _) = broadcast::channel(capacity.get());
        let (demand, _) = watch::channel(0);
        Self { sender, demand }
    }

    /// Publie une trame (synchrone, non bloquant, appelable hors runtime).
    ///
    /// Renvoie le nombre d'abonnés ; 0 n'est pas une erreur (trame jetée).
    pub fn publish(&self, frame: Frame) -> usize {
        // `send` échoue uniquement s'il n'y a aucun récepteur : la trame est jetée.
        self.sender.send(frame).unwrap_or(0)
    }

    /// Crée un nouvel abonné qui recevra les trames publiées à partir de maintenant.
    ///
    /// Incrémente la demande : le superviseur est réveillé s'il attendait un premier
    /// client. Le récepteur est créé **avant** l'incrément, pour qu'aucune trame
    /// publiée en réponse à la demande ne soit manquée.
    pub fn subscribe(&self) -> FrameSubscriber {
        let receiver = self.sender.subscribe();
        // `send_modify` s'applique même sans récepteur `watch` (contrairement à `send`).
        self.demand.send_modify(|count| *count += 1);
        FrameSubscriber {
            receiver,
            demand: self.demand.clone(),
            skipped: 0,
        }
    }

    /// Nombre d'abonnés actuellement vivants (valeur du compteur de demande).
    pub fn subscriber_count(&self) -> usize {
        *self.demand.borrow()
    }

    /// Observateur de la demande, destiné au `CaptureSupervisor`.
    pub fn demand(&self) -> DemandReceiver {
        DemandReceiver {
            receiver: self.demand.subscribe(),
        }
    }
}

/// Abonné au flux de trames.
///
/// **RAII** : sa destruction désabonne le récepteur `broadcast` **et** décrémente la
/// demande ; c'est le seul chemin de décrément, donc le compteur ne peut pas dériver.
/// Volontairement non `Clone`.
#[derive(Debug)]
pub struct FrameSubscriber {
    receiver: broadcast::Receiver<Frame>,
    demand: watch::Sender<usize>,
    skipped: u64,
}

impl FrameSubscriber {
    /// Attend la prochaine trame.
    ///
    /// En cas de retard (`Lagged`), les trames perdues sont comptées et la lecture
    /// reprend avec la plus ancienne trame encore disponible. Renvoie `None` si le
    /// hub est fermé. Sûr à l'annulation (utilisable dans `tokio::select!`).
    pub async fn next_frame(&mut self) -> Option<Frame> {
        loop {
            match self.receiver.recv().await {
                Ok(frame) => return Some(frame),
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    self.skipped = self.skipped.saturating_add(count);
                    tracing::debug!(skipped = count, "slow subscriber skipped frames");
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }

    /// Nombre total de trames sautées par cet abonné (client lent).
    pub fn skipped_frames(&self) -> u64 {
        self.skipped
    }
}

impl Drop for FrameSubscriber {
    /// Décrémente la demande (saturant : ne peut pas passer sous zéro).
    fn drop(&mut self) {
        self.demand
            .send_modify(|count| *count = count.saturating_sub(1));
    }
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
    pub fn subscriber_count(&self) -> usize {
        *self.receiver.borrow()
    }

    /// Se résout dès qu'au moins un abonné existe (immédiatement si c'est déjà le cas).
    pub async fn wait_for_subscribers(&mut self) {
        self.wait_until(|count| count > 0).await;
    }

    /// Se résout dès que plus aucun abonné n'existe (immédiatement si c'est déjà le cas).
    pub async fn wait_for_no_subscribers(&mut self) {
        self.wait_until(|count| count == 0).await;
    }

    async fn wait_until(&mut self, condition: impl Fn(usize) -> bool) {
        // `wait_for` renvoie un verrou de lecture (`watch::Ref`) : on le relâche
        // immédiatement. Il n'échoue que si le hub entier a disparu ; la demande ne
        // peut alors plus changer, on attend donc indéfiniment (l'appelant combine
        // toujours cette attente avec le jeton d'annulation).
        let satisfied = self
            .receiver
            .wait_for(|count| condition(*count))
            .await
            .is_ok();
        if !satisfied {
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::Bytes;

    use super::*;
    use crate::domain::{Resolution, SequenceNumber};

    fn frame(sequence: u64) -> Frame {
        Frame::new(
            Bytes::from_static(b"\xFF\xD8\xFF\xD9"),
            Resolution::new(1, 1).unwrap(),
            SequenceNumber::new(sequence),
        )
    }

    #[test]
    fn publish_without_subscriber_is_not_an_error() {
        let hub = FrameHub::new(HubCapacity::DEFAULT);
        assert_eq!(hub.publish(frame(0)), 0);
    }

    #[tokio::test]
    async fn every_subscriber_receives_every_frame() {
        let hub = FrameHub::new(HubCapacity::DEFAULT);
        let mut first = hub.subscribe();
        let mut second = hub.subscribe();
        assert_eq!(hub.publish(frame(1)), 2);
        assert_eq!(first.next_frame().await.unwrap().sequence().get(), 1);
        assert_eq!(second.next_frame().await.unwrap().sequence().get(), 1);
    }

    #[test]
    fn demand_follows_subscriber_lifetimes() {
        let hub = FrameHub::new(HubCapacity::DEFAULT);
        let demand = hub.demand();
        assert_eq!(demand.subscriber_count(), 0);
        let first = hub.subscribe();
        let second = hub.subscribe();
        assert_eq!(hub.subscriber_count(), 2);
        assert_eq!(demand.subscriber_count(), 2);
        drop(first);
        assert_eq!(demand.subscriber_count(), 1);
        drop(second);
        assert_eq!(demand.subscriber_count(), 0);
    }

    #[tokio::test]
    async fn slow_subscriber_skips_frames_and_continues() {
        let hub = FrameHub::new(HubCapacity::new(2).unwrap());
        let mut slow = hub.subscribe();
        for sequence in 0..10 {
            hub.publish(frame(sequence));
        }
        // Capacité 2 : seules les 2 dernières trames restent ; 8 sont sautées.
        assert_eq!(slow.next_frame().await.unwrap().sequence().get(), 8);
        assert_eq!(slow.skipped_frames(), 8);
        assert_eq!(slow.next_frame().await.unwrap().sequence().get(), 9);
    }

    #[tokio::test]
    async fn subscriber_sees_end_of_stream_when_hub_is_gone() {
        let hub = FrameHub::new(HubCapacity::DEFAULT);
        let mut subscriber = hub.subscribe();
        drop(hub);
        assert!(subscriber.next_frame().await.is_none());
    }

    #[tokio::test]
    async fn demand_receiver_wakes_up_on_first_and_last_subscriber() {
        let hub = FrameHub::new(HubCapacity::DEFAULT);
        let mut demand = hub.demand();
        let waiter = tokio::spawn({
            let mut demand = demand.clone();
            async move { demand.wait_for_subscribers().await }
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        let subscriber = hub.subscribe();
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("woken up by the first subscriber")
            .unwrap();
        drop(subscriber);
        tokio::time::timeout(Duration::from_secs(5), demand.wait_for_no_subscribers())
            .await
            .expect("woken up when the last subscriber leaves");
    }

    #[test]
    fn hub_capacity_rejects_zero() {
        assert!(HubCapacity::new(0).is_none());
        assert_eq!(HubCapacity::default().get(), 4);
    }
}
