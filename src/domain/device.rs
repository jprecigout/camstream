//! Paramètres de capture indépendants de toute bibliothèque V4L2.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::frame::{Fps, JpegQuality, Resolution};

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
///
/// La négociation (`capture::negotiation`, ADR 0008) choisit le format effectif parmi
/// ceux que le périphérique énumère.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FormatPreference {
    /// Choix automatique (défaut) : MJPEG natif si proposé (aucun ré-encodage), sinon
    /// YUYV, sinon Bayer 8 bits (`BA81`, `GBRG`, `GRBG`, `RGGB`), convertis en JPEG.
    #[default]
    Auto,
    /// MJPEG exigé : erreur fatale si la webcam ne le propose pas.
    Mjpeg,
    /// YUYV exigé (conversion et encodage JPEG logiciels, coûteux en CPU).
    Yuyv,
    /// Bayer 8 bits exigé, premier motif proposé dans l'ordre `BA81`, `GBRG`, `GRBG`,
    /// `RGGB` (débayerisation et encodage JPEG logiciels).
    Bayer,
}

impl FormatPreference {
    /// Nom stable (`auto`, `mjpeg`, `yuyv`, `bayer`), identique à la valeur de
    /// configuration `CAMSTREAM_FORMAT`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Mjpeg => "mjpeg",
            Self::Yuyv => "yuyv",
            Self::Bayer => "bayer",
        }
    }
}

/// Affiche le nom stable de la préférence.
impl fmt::Display for FormatPreference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Balance des blancs appliquée après débayerisation (formats Bayer uniquement : un
/// capteur brut n'a pas de processeur d'image qui la ferait).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum WhiteBalance {
    /// Hypothèse « monde gris » : gains rouge et bleu ramenant les moyennes des trois
    /// canaux à celle du vert, lissés d'une trame à l'autre (défaut).
    #[default]
    GrayWorld,
    /// Aucune correction (couleurs brutes du capteur).
    Off,
}

/// Paramètres de l'encodage JPEG logiciel des formats bruts (ignorés en MJPEG natif).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct EncodingSettings {
    /// Qualité JPEG (`CAMSTREAM_JPEG_QUALITY`, défaut 80).
    pub quality: JpegQuality,
    /// Balance des blancs des formats Bayer (`CAMSTREAM_WHITE_BALANCE`, défaut monde gris).
    pub white_balance: WhiteBalance,
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
    /// Résolution demandée : la négociation retient la taille proposée la plus proche
    /// (ADR 0008) et le pilote peut encore l'ajuster.
    pub resolution: Resolution,
    /// Cadence demandée.
    pub fps: Fps,
    /// Format de pixels souhaité.
    pub format: FormatPreference,
    /// Encodage JPEG logiciel des formats bruts.
    pub encoding: EncodingSettings,
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
        assert_eq!(FormatPreference::Bayer.to_string(), "bayer");
        let encoding = EncodingSettings::default();
        assert_eq!(encoding.quality.get(), 80);
        assert_eq!(encoding.white_balance, WhiteBalance::GrayWorld);
    }
}
