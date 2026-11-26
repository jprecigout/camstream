//! État observable de la capture, publié par le superviseur via `tokio::sync::watch`.

use tokio::sync::watch;

use crate::domain::SourceDescription;

/// État observable de la capture, publié via `tokio::sync::watch`.
///
/// Variante « dernière valeur » du pattern **Observer** : `/health`, les logs et
/// `main` lisent toujours l'état courant. Aucun `serde` : `/health` le convertit en
/// DTO (`server::health::HealthReport`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceStatus {
    /// Sonde de démarrage ou ouverture de la source en cours.
    Starting,
    /// Aucun abonné : webcam fermée, en attente du premier client.
    Idle,
    /// Trames en cours de production (y compris pendant le délai de grâce).
    Streaming {
        /// Paramètres effectivement négociés par la source.
        source: SourceDescription,
    },
    /// Source perdue alors que des clients attendent ; nouvelle tentative prévue.
    Reconnecting {
        /// Numéro de la tentative en cours (à partir de 1).
        attempt: u32,
        /// Message de la dernière erreur de capture.
        last_error: String,
    },
    /// Erreur fatale : l'application va s'arrêter.
    Failed {
        /// Cause de l'échec (erreur fatale ou fin anormale du thread de capture).
        reason: String,
    },
    /// Arrêt demandé et terminé.
    Stopped,
}

impl SourceStatus {
    /// Vrai pour l'état `Failed` (utilisé par `main` et `/health`).
    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

/// Récepteur de l'état de la capture (alias pour la lisibilité des signatures).
pub type StatusReceiver = watch::Receiver<SourceStatus>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_failed_is_failed() {
        assert!(SourceStatus::Failed { reason: "x".into() }.is_failed());
        for status in [
            SourceStatus::Starting,
            SourceStatus::Idle,
            SourceStatus::Stopped,
            SourceStatus::Reconnecting {
                attempt: 1,
                last_error: "e".into(),
            },
        ] {
            assert!(!status.is_failed(), "{status:?}");
        }
    }
}
