//! Ports (traits) de capture : abstractions que le cœur utilise sans connaître V4L2.

use std::fmt;

use super::device::{DevicePath, FormatPreference, SourceKind};
use super::frame::{Fps, Frame, PixelFormat, Resolution};

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
///
/// Pas de `#[from] std::io::Error` : chaque adaptateur traduit explicitement les
/// codes `errno` (`ENODEV` → [`CaptureError::Disconnected`], etc.) au lieu de tout
/// ranger dans [`CaptureError::Io`] par un `?` implicite.
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
    /// Le nœud n'est pas un périphérique de capture vidéo en streaming (par exemple
    /// le nœud de métadonnées `/dev/video1` d'une webcam UVC).
    #[error("not a streaming video capture device: {0}")]
    NotACaptureDevice(DevicePath),
    /// Aucun format compatible avec la préférence demandée.
    #[error("no supported pixel format for preference {preference:?}")]
    UnsupportedFormat {
        /// Préférence de format demandée par la configuration.
        preference: FormatPreference,
    },
    /// Le type de source demandé n'est pas compilé dans ce binaire (feature Cargo absente).
    #[error("source kind {0} is not available in this build")]
    SourceNotAvailable(SourceKind),
    /// Aucune trame reçue dans le délai imparti.
    #[error("timed out waiting for a frame")]
    Timeout,
    /// Trame invalide (JPEG tronqué, tampon vide) : on la jette.
    #[error("corrupted frame")]
    CorruptedFrame,
    /// Autre erreur d'entrée/sortie du pilote.
    #[error("video I/O error")]
    Io(#[source] std::io::Error),
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
    /// Classe l'erreur selon la politique de reprise (architecture § 6).
    ///
    /// `Timeout` mène à `Reopen` : avec `v4l` 0.14, un flux qui a expiré n'est plus
    /// réutilisable (`EINVAL` au `QBUF` suivant), et une webcam muette pendant le
    /// délai de lecture est considérée comme figée.
    pub fn recovery(&self) -> Recovery {
        match self {
            Self::CorruptedFrame => Recovery::Skip,
            Self::DeviceNotFound(_) | Self::Disconnected | Self::Timeout | Self::Io(_) => {
                Recovery::Reopen
            }
            Self::PermissionDenied(_)
            | Self::NotACaptureDevice(_)
            | Self::UnsupportedFormat { .. }
            | Self::SourceNotAvailable(_) => Recovery::Fatal,
        }
    }
}

/// Caractéristiques effectives d'une source démarrée.
///
/// Défini dans `domain` (et non dans `stream`) car il est renvoyé par le port
/// `FrameSource::description` : le placer dans `stream` créerait une dépendance
/// `domain` → `stream` interdite. Pas de `serde` ici : la forme JSON est un DTO de
/// `server::health`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceDescription {
    /// Nom lisible (ex. carte de la webcam, « synthetic test pattern »).
    pub name: String,
    /// Format natif négocié.
    pub pixel_format: PixelFormat,
    /// Résolution effective.
    pub resolution: Resolution,
    /// Cadence effective.
    pub fps: Fps,
}

/// Résumé sur une ligne, pour les logs (ex. « Webcam (MJPEG 640x480 @ 15 fps) »).
impl fmt::Display for SourceDescription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({} {} @ {} fps)",
            self.name, self.pixel_format, self.resolution, self.fps
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_policy_matches_the_architecture() {
        let path = || DevicePath::new("/dev/video0");
        let cases = [
            (CaptureError::CorruptedFrame, Recovery::Skip),
            (CaptureError::Timeout, Recovery::Reopen),
            (CaptureError::Disconnected, Recovery::Reopen),
            (CaptureError::DeviceNotFound(path()), Recovery::Reopen),
            (
                CaptureError::Io(std::io::Error::other("boom")),
                Recovery::Reopen,
            ),
            (CaptureError::PermissionDenied(path()), Recovery::Fatal),
            (CaptureError::NotACaptureDevice(path()), Recovery::Fatal),
            (
                CaptureError::UnsupportedFormat {
                    preference: FormatPreference::Auto,
                },
                Recovery::Fatal,
            ),
            (
                CaptureError::SourceNotAvailable(SourceKind::V4l2),
                Recovery::Fatal,
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(error.recovery(), expected, "{error}");
        }
    }

    #[test]
    fn error_messages_name_the_device() {
        let error = CaptureError::PermissionDenied(DevicePath::new("/dev/video0"));
        assert_eq!(
            error.to_string(),
            "permission denied on video device: /dev/video0"
        );
    }

    #[test]
    fn description_has_a_one_line_summary() {
        let description = SourceDescription {
            name: "Cam".into(),
            pixel_format: PixelFormat::Mjpeg,
            resolution: Resolution::new(640, 480).unwrap(),
            fps: Fps::new(15).unwrap(),
        };
        assert_eq!(description.to_string(), "Cam (MJPEG 640x480 @ 15 fps)");
    }
}
