//! Types métier décrivant une trame vidéo et ses caractéristiques.
//!
//! Ces types ne dépendent d'aucune bibliothèque technique (ni V4L2, ni HTTP) :
//! ils forment, avec les ports de [`super::source`], le vocabulaire du cœur.

use std::fmt;
use std::num::NonZeroU32;
use std::str::FromStr;
use std::time::{Duration, Instant};

use bytes::Bytes;

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
    pub const fn new(width: u32, height: u32) -> Option<Self> {
        match (NonZeroU32::new(width), NonZeroU32::new(height)) {
            (Some(width), Some(height)) => Some(Self { width, height }),
            _ => None,
        }
    }

    /// Largeur en pixels.
    pub fn width(&self) -> u32 {
        self.width.get()
    }

    /// Hauteur en pixels.
    pub fn height(&self) -> u32 {
        self.height.get()
    }
}

/// Affiche « LARGEURxHAUTEUR », forme réciproque de [`FromStr`].
impl fmt::Display for Resolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

/// Erreur d'analyse d'une [`Resolution`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "invalid resolution {input:?}: expected WIDTHxHEIGHT with non-zero dimensions (e.g. 640x480)"
)]
pub struct ParseResolutionError {
    /// Texte refusé.
    pub input: String,
}

/// Analyse « LARGEURxHAUTEUR » (ex. « 1280x720 », `X` majuscule accepté).
impl FromStr for Resolution {
    type Err = ParseResolutionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let error = || ParseResolutionError {
            input: s.to_owned(),
        };
        let (width, height) = s.trim().split_once(['x', 'X']).ok_or_else(error)?;
        let width = width.parse().map_err(|_| error())?;
        let height = height.parse().map_err(|_| error())?;
        Self::new(width, height).ok_or_else(error)
    }
}

/// Nombre d'images par seconde demandé ou négocié (**Newtype**, non nul).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fps(NonZeroU32);

impl Fps {
    /// Construit une cadence ; `None` si elle est nulle.
    pub const fn new(fps: u32) -> Option<Self> {
        match NonZeroU32::new(fps) {
            Some(fps) => Some(Self(fps)),
            None => None,
        }
    }

    /// Nombre d'images par seconde.
    pub fn get(self) -> u32 {
        self.0.get()
    }

    /// Intervalle théorique entre deux images (`1 / fps`).
    pub fn frame_interval(self) -> Duration {
        Duration::from_secs(1) / self.0.get()
    }
}

/// Affiche le nombre d'images par seconde.
impl fmt::Display for Fps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Numéro de séquence monotone d'une trame, attribué par la source (**Newtype**).
///
/// Permet aux clients et aux logs de détecter les trames sautées.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SequenceNumber(u64);

impl SequenceNumber {
    /// Construit un numéro de séquence.
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    /// Valeur brute.
    pub fn get(self) -> u64 {
        self.0
    }

    /// Numéro suivant (saturant : 2⁶⁴ trames ne seront jamais atteintes).
    #[must_use]
    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

/// Disposition des filtres colorés d'un capteur Bayer : couleurs des deux premiers
/// pixels de la première ligne puis de la deuxième ligne (motif 2x2 répété).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BayerPattern {
    /// `B G / G R` (FourCC V4L2 `BA81`).
    Bggr,
    /// `G B / R G` (FourCC `GBRG`).
    Gbrg,
    /// `G R / B G` (FourCC `GRBG`).
    Grbg,
    /// `R G / G B` (FourCC `RGGB`).
    Rggb,
}

/// Format de pixels négocié avec le périphérique.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// Images JPEG compressées par la webcam (format privilégié, sans ré-encodage).
    Mjpeg,
    /// Format brut YUV 4:2:2 entrelacé (`Y0 U Y1 V`), converti puis encodé en JPEG
    /// par logiciel (ADR 0008).
    Yuyv,
    /// Mosaïque Bayer brute 8 bits par photosite, débayerisée puis encodée en JPEG par
    /// logiciel (ADR 0008).
    Bayer8(BayerPattern),
}

impl PixelFormat {
    /// Nom court stable, utilisé dans les logs et `/health` : `MJPEG`, `YUYV`, ou le
    /// FourCC V4L2 du format Bayer (`BA81`, `GBRG`, `GRBG`, `RGGB`), tel qu'affiché par
    /// `v4l2-ctl --list-formats-ext`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mjpeg => "MJPEG",
            Self::Yuyv => "YUYV",
            Self::Bayer8(BayerPattern::Bggr) => "BA81",
            Self::Bayer8(BayerPattern::Gbrg) => "GBRG",
            Self::Bayer8(BayerPattern::Grbg) => "GRBG",
            Self::Bayer8(BayerPattern::Rggb) => "RGGB",
        }
    }

    /// Vrai si les trames du périphérique sont déjà des JPEG (aucun encodage logiciel).
    pub fn is_compressed(self) -> bool {
        self == Self::Mjpeg
    }
}

/// Affiche le nom court du format.
impl fmt::Display for PixelFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Qualité d'encodage JPEG logiciel, de 1 (plus compact) à 100 (meilleure image)
/// (**Newtype**).
///
/// Sous 90, l'encodeur sous-échantillonne la chrominance (4:2:0), ce qui réduit le
/// coût CPU et la taille des trames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JpegQuality(u8);

impl JpegQuality {
    /// Qualité par défaut : 80, bon compromis netteté / débit / CPU sur Raspberry Pi.
    pub const DEFAULT: Self = Self(80);

    /// Construit une qualité ; `None` hors de `1..=100`.
    pub const fn new(quality: u8) -> Option<Self> {
        if matches!(quality, 1..=100) {
            Some(Self(quality))
        } else {
            None
        }
    }

    /// Valeur de 1 à 100.
    pub fn get(self) -> u8 {
        self.0
    }
}

impl Default for JpegQuality {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Affiche la valeur numérique.
impl fmt::Display for JpegQuality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
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
    ///
    /// L'instant de capture est l'instant de construction (horloge monotone).
    pub fn new(data: Bytes, resolution: Resolution, sequence: SequenceNumber) -> Self {
        Self {
            data,
            resolution,
            sequence,
            captured_at: Instant::now(),
        }
    }

    /// Construit une trame en précisant l'instant de capture : celui du retrait du
    /// tampon du pilote, **avant** une éventuelle conversion logicielle, pour que la
    /// latence mesurée inclue le coût de l'encodage JPEG.
    pub fn captured(
        data: Bytes,
        resolution: Resolution,
        sequence: SequenceNumber,
        captured_at: Instant,
    ) -> Self {
        Self {
            data,
            resolution,
            sequence,
            captured_at,
        }
    }

    /// Données JPEG (partagées, sans copie).
    pub fn data(&self) -> &Bytes {
        &self.data
    }

    /// Résolution de l'image.
    pub fn resolution(&self) -> Resolution {
        self.resolution
    }

    /// Numéro de séquence attribué par la source.
    pub fn sequence(&self) -> SequenceNumber {
        self.sequence
    }

    /// Instant de capture (horloge monotone), utile pour mesurer la latence.
    pub fn captured_at(&self) -> Instant {
        self.captured_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolution_parses_and_displays() {
        let resolution: Resolution = "1280x720".parse().unwrap();
        assert_eq!((resolution.width(), resolution.height()), (1280, 720));
        assert_eq!(resolution.to_string(), "1280x720");
        assert_eq!(
            "640X480".parse::<Resolution>().unwrap().to_string(),
            "640x480"
        );
        assert_eq!(" 320x240 ".parse::<Resolution>().unwrap().width(), 320);
    }

    #[test]
    fn resolution_rejects_invalid_input() {
        for input in [
            "",
            "640",
            "640x",
            "x480",
            "0x480",
            "640x0",
            "-1x2",
            "axb",
            "640x480x2",
        ] {
            let error = input.parse::<Resolution>().unwrap_err();
            assert_eq!(error.input, input, "entrée {input:?}");
        }
        assert!(Resolution::new(0, 1).is_none());
    }

    #[test]
    fn fps_is_non_zero_and_gives_interval() {
        assert!(Fps::new(0).is_none());
        let fps = Fps::new(25).unwrap();
        assert_eq!(fps.get(), 25);
        assert_eq!(fps.frame_interval(), Duration::from_millis(40));
        assert_eq!(fps.to_string(), "25");
    }

    #[test]
    fn sequence_number_is_monotonic_and_saturating() {
        let first = SequenceNumber::default();
        assert!(first.next() > first);
        assert_eq!(SequenceNumber::new(u64::MAX).next().get(), u64::MAX);
    }

    #[test]
    fn frame_clone_shares_the_jpeg_bytes() {
        let data = Bytes::from_static(b"\xFF\xD8jpeg\xFF\xD9");
        let frame = Frame::new(
            data.clone(),
            Resolution::new(2, 2).unwrap(),
            SequenceNumber::new(7),
        );
        let clone = frame.clone();
        // Même zone mémoire : le clonage ne copie pas l'image.
        assert_eq!(clone.data().as_ptr(), frame.data().as_ptr());
        assert_eq!(clone.sequence().get(), 7);
        assert_eq!(PixelFormat::Mjpeg.to_string(), "MJPEG");
    }

    #[test]
    fn pixel_formats_have_v4l2_names() {
        let names: Vec<_> = [PixelFormat::Mjpeg, PixelFormat::Yuyv]
            .into_iter()
            .chain(
                [
                    BayerPattern::Bggr,
                    BayerPattern::Gbrg,
                    BayerPattern::Grbg,
                    BayerPattern::Rggb,
                ]
                .map(PixelFormat::Bayer8),
            )
            .map(PixelFormat::as_str)
            .collect();
        assert_eq!(names, ["MJPEG", "YUYV", "BA81", "GBRG", "GRBG", "RGGB"]);
        assert!(PixelFormat::Mjpeg.is_compressed());
        assert!(!PixelFormat::Bayer8(BayerPattern::Grbg).is_compressed());
    }

    #[test]
    fn jpeg_quality_is_bounded() {
        assert!(JpegQuality::new(0).is_none());
        assert!(JpegQuality::new(101).is_none());
        assert_eq!(JpegQuality::new(1).unwrap().get(), 1);
        assert_eq!(JpegQuality::new(100).unwrap().get(), 100);
        assert_eq!(JpegQuality::default().get(), 80);
    }
}
