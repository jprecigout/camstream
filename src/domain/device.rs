//! Paramètres de capture indépendants de toute bibliothèque V4L2.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::frame::{Fps, Resolution};

/// Chemin d'un périphérique vidéo (ex. `/dev/video0`) (**Newtype**).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DevicePath(PathBuf);

impl DevicePath {
    /// Construit un chemin de périphérique.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    /// Chemin sous-jacent.
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

/// Affichage lisible du chemin (`PathBuf` n'implémente pas `Display`).
impl fmt::Display for DevicePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0.display(), f)
    }
}

/// Type de source à instancier par la fabrique.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceKind {
    /// Webcam réelle via V4L2.
    V4l2,
    /// Mire de test JPEG générée sans matériel (démonstration, CI).
    Synthetic,
}

impl SourceKind {
    /// Nom stable (`v4l2`, `synthetic`), identique à la valeur de configuration.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V4l2 => "v4l2",
            Self::Synthetic => "synthetic",
        }
    }
}

/// Affiche le nom stable du type de source.
impl fmt::Display for SourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Préférence de format de pixels demandée par la configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_path_displays_the_path() {
        let path = DevicePath::new("/dev/video0");
        assert_eq!(path.to_string(), "/dev/video0");
        assert_eq!(path.as_path(), Path::new("/dev/video0"));
    }

    #[test]
    fn source_kind_names_are_stable() {
        assert_eq!(SourceKind::V4l2.to_string(), "v4l2");
        assert_eq!(SourceKind::Synthetic.to_string(), "synthetic");
        assert_eq!(FormatPreference::default(), FormatPreference::Auto);
    }
}
