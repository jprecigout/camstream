//! Négociation du format, de la résolution et de la cadence à partir des capacités
//! **énumérées** d'un périphérique (ADR 0008).
//!
//! Ce module ne dépend ni de V4L2 ni d'aucune crate : l'adaptateur V4L2 implémente le
//! port [`CapabilityQuery`] au-dessus des ioctl `VIDIOC_ENUM_*`, la source synthétique
//! et les tests utilisent une [`CapabilityTable`]. Les règles de choix sont des
//! fonctions pures, testées sur des tables simulées.
//!
//! Stratégie :
//! 1. **format** : MJPEG natif d'abord (aucun ré-encodage), puis YUYV, puis Bayer 8 bits
//!    dans l'ordre `BA81`, `GBRG`, `GRBG`, `RGGB` ([`choose_format`]) ;
//! 2. **résolution** : la plus grande taille proposée qui tient dans la résolution
//!    demandée (largeur **et** hauteur), sinon la plus petite au-dessus
//!    ([`choose_resolution`]) ;
//! 3. **cadence** : la cadence proposée la plus proche de la demande, la plus basse en
//!    cas d'égalité ([`choose_fps`]).
//!
//! Une énumération de tailles ou de cadences non prise en charge par le pilote n'est
//! pas une erreur : la valeur demandée est alors transmise telle quelle et le pilote
//! l'ajuste lui-même (comportement de la v1).

use std::cmp::Reverse;
use std::fmt::{self, Write as _};

use crate::domain::{BayerPattern, CaptureError, CaptureSettings, Fps, PixelFormat, Resolution};

/// Code à quatre caractères identifiant un format de pixels V4L2 (**Newtype**).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FourCc([u8; 4]);

impl FourCc {
    /// MJPEG (`MJPG`).
    pub const MJPG: Self = Self(*b"MJPG");
    /// JPEG (`JPEG`), variante rare mais équivalente pour la diffusion.
    pub const JPEG: Self = Self(*b"JPEG");
    /// YUV 4:2:2 entrelacé (`YUYV`).
    pub const YUYV: Self = Self(*b"YUYV");
    /// Bayer 8 bits `BGGR` (`BA81`).
    pub const BA81: Self = Self(*b"BA81");
    /// Bayer 8 bits `GBRG`.
    pub const GBRG: Self = Self(*b"GBRG");
    /// Bayer 8 bits `GRBG`.
    pub const GRBG: Self = Self(*b"GRBG");
    /// Bayer 8 bits `RGGB`.
    pub const RGGB: Self = Self(*b"RGGB");

    /// Construit un FourCC à partir de ses quatre octets.
    pub const fn new(repr: [u8; 4]) -> Self {
        Self(repr)
    }

    /// Octets du code.
    pub fn repr(self) -> [u8; 4] {
        self.0
    }

    /// Format pris en charge correspondant, `None` pour un format inconnu de Camstream
    /// (H.264, Bayer 10 bits…).
    pub fn pixel_format(self) -> Option<PixelFormat> {
        match self {
            Self::MJPG | Self::JPEG => Some(PixelFormat::Mjpeg),
            Self::YUYV => Some(PixelFormat::Yuyv),
            Self::BA81 => Some(PixelFormat::Bayer8(BayerPattern::Bggr)),
            Self::GBRG => Some(PixelFormat::Bayer8(BayerPattern::Gbrg)),
            Self::GRBG => Some(PixelFormat::Bayer8(BayerPattern::Grbg)),
            Self::RGGB => Some(PixelFormat::Bayer8(BayerPattern::Rggb)),
            _ => None,
        }
    }
}

/// Texte du code, sans échouer sur des octets non UTF-8 (remplacés par `�`).
impl fmt::Display for FourCc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.0))
    }
}

/// Même forme que `Display` (`GRBG`) : plus lisible que le tableau d'octets.
impl fmt::Debug for FourCc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FourCc({self})")
    }
}

/// Tailles d'image proposées pour un format (`VIDIOC_ENUM_FRAMESIZES`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeRange {
    /// Une taille unique.
    Discrete(Resolution),
    /// Toutes les tailles de `min` à `max` par pas de `step_width` × `step_height`
    /// (un pas de 1 correspond à une plage continue).
    Stepwise {
        /// Plus petite taille.
        min: Resolution,
        /// Plus grande taille.
        max: Resolution,
        /// Pas horizontal (0 est traité comme 1).
        step_width: u32,
        /// Pas vertical (0 est traité comme 1).
        step_height: u32,
    },
}

impl SizeRange {
    /// Meilleure taille de la plage pour une demande : la demande ramenée dans la plage
    /// puis arrondie au pas inférieur (elle ne dépasse donc la demande que si le
    /// minimum de la plage la dépasse).
    pub fn closest_to(&self, requested: Resolution) -> Resolution {
        match *self {
            Self::Discrete(size) => size,
            Self::Stepwise {
                min,
                max,
                step_width,
                step_height,
            } => {
                let snap = |wanted: u32, min: u32, max: u32, step: u32| {
                    let step = step.max(1);
                    let max = max.max(min);
                    let clamped = wanted.clamp(min, max);
                    min + (clamped - min) / step * step
                };
                let width = snap(requested.width(), min.width(), max.width(), step_width);
                let height = snap(requested.height(), min.height(), max.height(), step_height);
                // `min` est non nul et le résultat est ≥ `min` : jamais nul.
                Resolution::new(width, height).unwrap_or(min)
            }
        }
    }
}

/// Forme compacte : `324x240` ou `160x120-1280x720/16x8`.
impl fmt::Display for SizeRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Discrete(size) => write!(f, "{size}"),
            Self::Stepwise {
                min,
                max,
                step_width,
                step_height,
            } => write!(f, "{min}-{max}/{step_width}x{step_height}"),
        }
    }
}

/// Intervalle entre deux images, en secondes (`numerator / denominator`), tel que
/// renvoyé par V4L2 (`1/15` pour 15 i/s).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameInterval {
    /// Numérateur (secondes).
    pub numerator: u32,
    /// Dénominateur.
    pub denominator: u32,
}

impl FrameInterval {
    /// Construit un intervalle.
    pub const fn new(numerator: u32, denominator: u32) -> Self {
        Self {
            numerator,
            denominator,
        }
    }

    /// Cadence exacte correspondante ; `None` si l'intervalle est dégénéré.
    pub fn rate(self) -> Option<f64> {
        (self.numerator != 0 && self.denominator != 0)
            .then(|| f64::from(self.denominator) / f64::from(self.numerator))
    }

    /// Cadence entière arrondie au plus proche (`1001/30000`, soit 29,97 i/s, donne
    /// 30) ; `None` si l'intervalle est dégénéré ou donne moins de 0,5 i/s.
    pub fn to_fps(self) -> Option<Fps> {
        if self.numerator == 0 {
            return None;
        }
        let fps = (u64::from(self.denominator) + u64::from(self.numerator) / 2)
            / u64::from(self.numerator);
        Fps::new(u32::try_from(fps).ok()?)
    }
}

/// Cadences proposées pour un format et une taille (`VIDIOC_ENUM_FRAMEINTERVALS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntervalRange {
    /// Un intervalle unique.
    Discrete(FrameInterval),
    /// Tout intervalle de `min` (cadence maximale) à `max` (cadence minimale) ; le pas
    /// est ignoré, le pilote arrondit lui-même.
    Stepwise {
        /// Intervalle le plus court (cadence la plus haute).
        min: FrameInterval,
        /// Intervalle le plus long (cadence la plus basse).
        max: FrameInterval,
    },
}

impl IntervalRange {
    /// Cadence de la plage la plus proche de `requested` ; `None` si la plage est
    /// dégénérée.
    fn closest_rate(&self, requested: f64) -> Option<f64> {
        match *self {
            Self::Discrete(interval) => interval.rate(),
            Self::Stepwise { min, max } => {
                let (low, high) = (max.rate()?, min.rate()?);
                Some(requested.clamp(low.min(high), high.max(low)))
            }
        }
    }
}

/// Port d'interrogation des capacités d'un périphérique.
///
/// Implémente le pattern **Strategy** (et **Adapter** côté V4L2) : la négociation ne
/// connaît que ce trait ; l'adaptateur V4L2 l'implémente sur les ioctl d'énumération,
/// [`CapabilityTable`] sur une table en mémoire (tests, source synthétique).
pub trait CapabilityQuery {
    /// Formats proposés, dans l'ordre du pilote.
    fn formats(&self) -> Result<Vec<FourCc>, CaptureError>;
    /// Tailles proposées pour un format (erreur si le pilote ne sait pas les énumérer).
    fn sizes(&self, fourcc: FourCc) -> Result<Vec<SizeRange>, CaptureError>;
    /// Cadences proposées pour un format et une taille (erreur si le pilote ne sait pas
    /// les énumérer).
    fn intervals(
        &self,
        fourcc: FourCc,
        size: Resolution,
    ) -> Result<Vec<IntervalRange>, CaptureError>;
}

/// Capacités décrites en mémoire : implémentation de [`CapabilityQuery`] pour les tests
/// et la source synthétique (qui imite ainsi une vraie caméra).
///
/// Construite par chaînage (style **Builder** léger) : `CapabilityTable::new()
/// .with_format(..)`. Les cadences sont communes à toutes les tailles d'un format ;
/// `None` simule un pilote qui ne sait pas énumérer ses cadences.
#[derive(Debug, Clone, Default)]
pub struct CapabilityTable {
    entries: Vec<TableEntry>,
}

/// Format d'une [`CapabilityTable`].
#[derive(Debug, Clone)]
struct TableEntry {
    fourcc: FourCc,
    sizes: Vec<SizeRange>,
    intervals: Option<Vec<IntervalRange>>,
}

impl CapabilityTable {
    /// Table vide (aucun format).
    pub fn new() -> Self {
        Self::default()
    }

    /// Ajoute un format, ses tailles et ses cadences (`None` : énumération des cadences
    /// non prise en charge).
    #[must_use]
    pub fn with_format(
        mut self,
        fourcc: FourCc,
        sizes: impl IntoIterator<Item = SizeRange>,
        intervals: Option<Vec<IntervalRange>>,
    ) -> Self {
        self.entries.push(TableEntry {
            fourcc,
            sizes: sizes.into_iter().collect(),
            intervals,
        });
        self
    }

    /// Capacités relevées sur la caméra de l'utilisateur (Raspberry Pi, 2026-11-27) :
    /// uniquement `GRBG` (Bayer 8 bits) en 162x120 et 324x240, cadences non connues.
    pub fn grbg_camera() -> Self {
        let size = |width, height| {
            // Constantes non nulles : `Resolution::new` ne peut pas échouer.
            Resolution::new(width, height).map(SizeRange::Discrete)
        };
        Self::new().with_format(
            FourCc::GRBG,
            [size(162, 120), size(324, 240)].into_iter().flatten(),
            None,
        )
    }

    fn entry(&self, fourcc: FourCc) -> Option<&TableEntry> {
        self.entries.iter().find(|entry| entry.fourcc == fourcc)
    }
}

impl CapabilityQuery for CapabilityTable {
    fn formats(&self) -> Result<Vec<FourCc>, CaptureError> {
        Ok(self.entries.iter().map(|entry| entry.fourcc).collect())
    }

    fn sizes(&self, fourcc: FourCc) -> Result<Vec<SizeRange>, CaptureError> {
        Ok(self
            .entry(fourcc)
            .map(|entry| entry.sizes.clone())
            .unwrap_or_default())
    }

    fn intervals(
        &self,
        fourcc: FourCc,
        _size: Resolution,
    ) -> Result<Vec<IntervalRange>, CaptureError> {
        self.entry(fourcc)
            .and_then(|entry| entry.intervals.clone())
            .ok_or_else(|| CaptureError::Io(std::io::Error::from(std::io::ErrorKind::Unsupported)))
    }
}

/// Résultat de la négociation, à appliquer au périphérique (`S_FMT`, `S_PARM`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Negotiated {
    /// FourCC exact à demander (celui énuméré par le pilote).
    pub fourcc: FourCc,
    /// Format correspondant.
    pub pixel_format: PixelFormat,
    /// Résolution choisie.
    pub resolution: Resolution,
    /// Cadence choisie.
    pub fps: Fps,
}

/// Ordre de préférence des motifs Bayer quand plusieurs sont proposés (ordre des
/// FourCC V4L2 : `BA81`, `GBRG`, `GRBG`, `RGGB`). Le coût de conversion est identique.
const BAYER_PREFERENCE: [BayerPattern; 4] = [
    BayerPattern::Bggr,
    BayerPattern::Gbrg,
    BayerPattern::Grbg,
    BayerPattern::Rggb,
];

/// Ordre de préférence des formats pour une préférence de configuration.
fn preference_order(preference: crate::domain::FormatPreference) -> Vec<PixelFormat> {
    use crate::domain::FormatPreference;
    let bayer = BAYER_PREFERENCE.map(PixelFormat::Bayer8);
    match preference {
        FormatPreference::Auto => [PixelFormat::Mjpeg, PixelFormat::Yuyv]
            .into_iter()
            .chain(bayer)
            .collect(),
        FormatPreference::Mjpeg => vec![PixelFormat::Mjpeg],
        FormatPreference::Yuyv => vec![PixelFormat::Yuyv],
        FormatPreference::Bayer => bayer.to_vec(),
    }
}

/// Choisit le format selon la préférence : premier format de l'ordre de préférence
/// proposé par le périphérique ; `None` si aucun ne convient.
pub fn choose_format(
    offered: &[PixelFormat],
    preference: crate::domain::FormatPreference,
) -> Option<PixelFormat> {
    preference_order(preference)
        .into_iter()
        .find(|format| offered.contains(format))
}

/// Choisit la résolution : parmi les meilleures tailles de chaque plage
/// ([`SizeRange::closest_to`]), la **plus grande surface qui tient dans la demande**
/// (largeur et hauteur ≤ demandées) ; à défaut, la **plus petite surface au-dessus**.
/// À surface égale, le rapport largeur/hauteur le plus proche de la demande l'emporte.
/// `None` si aucune taille n'est énumérée.
///
/// Tenir dans la demande (plutôt que comparer des surfaces seules) évite de retenir
/// une taille plus large que demandée, et privilégie le coût CPU de la conversion.
pub fn choose_resolution(sizes: &[SizeRange], requested: Resolution) -> Option<Resolution> {
    let area = |size: Resolution| u64::from(size.width()) * u64::from(size.height());
    // Écart de rapport d'aspect sans flottant : |w·H − h·W|.
    let aspect_gap = |size: Resolution| {
        (u64::from(size.width()) * u64::from(requested.height()))
            .abs_diff(u64::from(size.height()) * u64::from(requested.width()))
    };
    let candidates = sizes.iter().map(|range| range.closest_to(requested));
    let fits = |size: &Resolution| {
        size.width() <= requested.width() && size.height() <= requested.height()
    };
    candidates
        .clone()
        .filter(fits)
        .max_by_key(|&size| (area(size), Reverse(aspect_gap(size))))
        .or_else(|| candidates.min_by_key(|&size| (area(size), aspect_gap(size))))
}

/// Choisit la cadence la plus proche de `requested` parmi les cadences proposées ; en
/// cas d'égalité, la plus basse (moins de CPU et de débit). `None` si aucune cadence
/// exploitable n'est énumérée.
pub fn choose_fps(intervals: &[IntervalRange], requested: Fps) -> Option<Fps> {
    let wanted = f64::from(requested.get());
    let best = intervals
        .iter()
        .filter_map(|range| range.closest_rate(wanted))
        .filter(|rate| rate.is_finite() && *rate > 0.0)
        .min_by(|a, b| {
            (a - wanted)
                .abs()
                .total_cmp(&(b - wanted).abs())
                .then(a.total_cmp(b))
        })?;
    // Cadence entière (au moins 1) : le pilote retient l'intervalle le plus proche.
    let rounded = best.round().clamp(1.0, f64::from(u32::MAX));
    // `as` : valeur finie, arrondie et bornée à la plage de `u32` juste au-dessus.
    Fps::new(rounded as u32)
}

/// Format énuméré, avec ses tailles, pour le journal et les messages d'erreur.
#[derive(Debug, Clone)]
struct Offer {
    fourcc: FourCc,
    sizes: Vec<SizeRange>,
}

/// Résumé lisible des formats proposés : `GRBG 162x120, 324x240; H264 (unsupported)`.
fn describe(offers: &[Offer]) -> String {
    if offers.is_empty() {
        return "no format".to_owned();
    }
    let mut text = String::new();
    for (index, offer) in offers.iter().enumerate() {
        if index > 0 {
            text.push_str("; ");
        }
        // Les écritures dans une `String` n'échouent jamais.
        let _ = write!(text, "{}", offer.fourcc);
        if offer.fourcc.pixel_format().is_none() {
            text.push_str(" (unsupported)");
        }
        for (position, size) in offer.sizes.iter().enumerate() {
            text.push_str(if position == 0 { " " } else { ", " });
            let _ = write!(text, "{size}");
        }
    }
    text
}

/// Négocie format, résolution et cadence pour `settings` à partir des capacités
/// énumérées par `query`, et journalise (niveau `info`) l'offre et le choix.
///
/// Erreurs : celle de l'énumération des formats (déconnexion, E/S), ou
/// [`CaptureError::UnsupportedFormat`] (fatale) avec la liste de ce que propose la
/// caméra si aucun format ne correspond à la préférence. Une énumération des tailles
/// ou des cadences en échec n'est pas une erreur (valeur demandée transmise au pilote),
/// sauf une déconnexion.
///
/// Une liste de formats **vide** donne une erreur d'E/S (reprise `Reopen`) et non
/// `UnsupportedFormat` : `v4l` renvoie une liste vide quand le premier
/// `VIDIOC_ENUM_FMT` échoue, par exemple si la webcam est débranchée pendant
/// l'ouverture ; en faire une erreur fatale arrêterait l'application pour un incident
/// passager.
pub fn negotiate(
    query: &dyn CapabilityQuery,
    settings: &CaptureSettings,
) -> Result<Negotiated, CaptureError> {
    let offers = query
        .formats()?
        .into_iter()
        .map(|fourcc| {
            let sizes = match query.sizes(fourcc) {
                Ok(sizes) => sizes,
                Err(CaptureError::Disconnected) => return Err(CaptureError::Disconnected),
                Err(error) => {
                    tracing::debug!(%fourcc, %error, "frame sizes not enumerable");
                    Vec::new()
                }
            };
            Ok(Offer { fourcc, sizes })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if offers.is_empty() {
        return Err(CaptureError::Io(std::io::Error::other(
            "the device enumerated no pixel format",
        )));
    }
    let offered_text = describe(&offers);
    tracing::info!(offered = %offered_text, "camera formats detected");

    let supported: Vec<PixelFormat> = offers
        .iter()
        .filter_map(|offer| offer.fourcc.pixel_format())
        .collect();
    let preference = settings.format;
    let Some(pixel_format) = choose_format(&supported, preference) else {
        return Err(CaptureError::UnsupportedFormat {
            preference,
            offered: offered_text,
        });
    };
    // Invariant : `choose_format` ne renvoie qu'un format présent dans `offers`.
    let Some(offer) = offers
        .iter()
        .find(|offer| offer.fourcc.pixel_format() == Some(pixel_format))
    else {
        return Err(CaptureError::UnsupportedFormat {
            preference,
            offered: offered_text,
        });
    };

    let resolution = choose_resolution(&offer.sizes, settings.resolution).unwrap_or_else(|| {
        tracing::debug!("no frame size enumerated, the driver will adjust the request");
        settings.resolution
    });
    let fps = match query.intervals(offer.fourcc, resolution) {
        Ok(intervals) => choose_fps(&intervals, settings.fps),
        Err(CaptureError::Disconnected) => return Err(CaptureError::Disconnected),
        Err(error) => {
            tracing::debug!(%error, "frame intervals not enumerable");
            None
        }
    }
    .unwrap_or(settings.fps);

    let conversion = if pixel_format.is_compressed() {
        "none (native JPEG)"
    } else {
        "software JPEG encoding"
    };
    tracing::info!(
        format = %pixel_format,
        resolution = %resolution,
        fps = %fps,
        requested = %format_args!("{} {} @ {} fps", preference, settings.resolution, settings.fps),
        conversion,
        "capture format selected"
    );
    Ok(Negotiated {
        fourcc: offer.fourcc,
        pixel_format,
        resolution,
        fps,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::domain::{DevicePath, EncodingSettings, FormatPreference, SourceKind};

    fn res(width: u32, height: u32) -> Resolution {
        Resolution::new(width, height).unwrap()
    }

    fn fps(value: u32) -> Fps {
        Fps::new(value).unwrap()
    }

    fn discrete(sizes: &[(u32, u32)]) -> Vec<SizeRange> {
        sizes
            .iter()
            .map(|&(w, h)| SizeRange::Discrete(res(w, h)))
            .collect()
    }

    fn rates(values: &[u32]) -> Vec<IntervalRange> {
        values
            .iter()
            .map(|&value| IntervalRange::Discrete(FrameInterval::new(1, value)))
            .collect()
    }

    fn settings(format: FormatPreference, resolution: Resolution, rate: u32) -> CaptureSettings {
        CaptureSettings {
            kind: SourceKind::V4l2,
            device: DevicePath::new("/dev/video0"),
            resolution,
            fps: fps(rate),
            format,
            encoding: EncodingSettings::default(),
            read_timeout: Duration::from_secs(2),
        }
    }

    #[test]
    fn fourcc_maps_supported_formats() {
        assert_eq!(FourCc::MJPG.pixel_format(), Some(PixelFormat::Mjpeg));
        assert_eq!(FourCc::JPEG.pixel_format(), Some(PixelFormat::Mjpeg));
        assert_eq!(
            FourCc::GRBG.pixel_format(),
            Some(PixelFormat::Bayer8(BayerPattern::Grbg))
        );
        assert_eq!(
            FourCc::BA81.pixel_format(),
            Some(PixelFormat::Bayer8(BayerPattern::Bggr))
        );
        assert_eq!(FourCc::new(*b"H264").pixel_format(), None);
        assert_eq!(
            FourCc::new([0xFF, b'A', b'B', b'C']).to_string(),
            "\u{FFFD}ABC"
        );
        assert_eq!(format!("{:?}", FourCc::GRBG), "FourCc(GRBG)");
    }

    #[test]
    fn format_preference_order() {
        use PixelFormat::{Bayer8, Mjpeg, Yuyv};
        let grbg = Bayer8(BayerPattern::Grbg);
        let rggb = Bayer8(BayerPattern::Rggb);
        let all = [rggb, Yuyv, grbg, Mjpeg];
        assert_eq!(choose_format(&all, FormatPreference::Auto), Some(Mjpeg));
        assert_eq!(choose_format(&all[..3], FormatPreference::Auto), Some(Yuyv));
        // Ordre Bayer : BA81, GBRG, GRBG, RGGB, quel que soit l'ordre du pilote.
        assert_eq!(
            choose_format(&[rggb, grbg], FormatPreference::Auto),
            Some(grbg)
        );
        assert_eq!(choose_format(&all, FormatPreference::Bayer), Some(grbg));
        assert_eq!(choose_format(&all, FormatPreference::Yuyv), Some(Yuyv));
        assert_eq!(choose_format(&[grbg], FormatPreference::Mjpeg), None);
        assert_eq!(choose_format(&[Mjpeg], FormatPreference::Bayer), None);
        assert_eq!(choose_format(&[], FormatPreference::Auto), None);
    }

    #[test]
    fn resolution_fits_in_the_request_or_is_the_smallest_above() {
        let sizes = discrete(&[(162, 120), (324, 240)]);
        // Caméra de l'utilisateur : 640x480 demandé → 324x240.
        assert_eq!(
            choose_resolution(&sizes, res(640, 480)),
            Some(res(324, 240))
        );
        assert_eq!(
            choose_resolution(&sizes, res(324, 240)),
            Some(res(324, 240))
        );
        assert_eq!(
            choose_resolution(&sizes, res(323, 240)),
            Some(res(162, 120))
        );
        // Rien ne tient : la plus petite au-dessus.
        assert_eq!(
            choose_resolution(&sizes, res(100, 100)),
            Some(res(162, 120))
        );
        assert_eq!(choose_resolution(&[], res(640, 480)), None);

        let webcam = discrete(&[
            (1280, 720),
            (640, 480),
            (800, 600),
            (1920, 1080),
            (320, 240),
        ]);
        assert_eq!(
            choose_resolution(&webcam, res(640, 480)),
            Some(res(640, 480))
        );
        assert_eq!(
            choose_resolution(&webcam, res(1024, 768)),
            Some(res(800, 600))
        );
        // 1280x720 dépasse en hauteur 1280x600 : 800x600 tient.
        assert_eq!(
            choose_resolution(&webcam, res(1280, 600)),
            Some(res(800, 600))
        );
        assert_eq!(
            choose_resolution(&webcam, res(4000, 3000)),
            Some(res(1920, 1080))
        );
    }

    #[test]
    fn resolution_ties_prefer_the_requested_aspect_ratio() {
        // Même surface (76 800 pixels) : 320x240 (4:3) contre 480x160 (3:1).
        let sizes = discrete(&[(480, 160), (320, 240)]);
        assert_eq!(
            choose_resolution(&sizes, res(640, 480)),
            Some(res(320, 240))
        );
    }

    #[test]
    fn stepwise_sizes_snap_to_the_step() {
        let range = SizeRange::Stepwise {
            min: res(160, 120),
            max: res(1280, 960),
            step_width: 16,
            step_height: 8,
        };
        assert_eq!(range.closest_to(res(650, 485)), res(640, 480));
        assert_eq!(range.closest_to(res(100, 100)), res(160, 120));
        assert_eq!(range.closest_to(res(5000, 5000)), res(1280, 960));
        let continuous = SizeRange::Stepwise {
            min: res(2, 2),
            max: res(4000, 3000),
            step_width: 0,
            step_height: 1,
        };
        assert_eq!(continuous.closest_to(res(641, 479)), res(641, 479));
        assert_eq!(
            choose_resolution(&[range], res(1000, 700)),
            Some(res(992, 696))
        );
        assert_eq!(range.to_string(), "160x120-1280x960/16x8");
    }

    #[test]
    fn fps_is_the_closest_offered_rate() {
        assert_eq!(
            choose_fps(&rates(&[30, 25, 20, 15, 10, 5]), fps(15)),
            Some(fps(15))
        );
        assert_eq!(choose_fps(&rates(&[30, 10]), fps(15)), Some(fps(10)));
        assert_eq!(choose_fps(&rates(&[30, 5]), fps(60)), Some(fps(30)));
        // Égalité 10 / 20 pour 15 demandés : la plus basse.
        assert_eq!(choose_fps(&rates(&[20, 10]), fps(15)), Some(fps(10)));
        // 7,5 i/s (2/15) arrondi à 8.
        let half = [IntervalRange::Discrete(FrameInterval::new(2, 15))];
        assert_eq!(choose_fps(&half, fps(5)), Some(fps(8)));
        let stepwise = [IntervalRange::Stepwise {
            min: FrameInterval::new(1, 60),
            max: FrameInterval::new(1, 1),
        }];
        assert_eq!(choose_fps(&stepwise, fps(15)), Some(fps(15)));
        assert_eq!(choose_fps(&stepwise, fps(120)), Some(fps(60)));
        assert_eq!(choose_fps(&[], fps(15)), None);
        let degenerate = [IntervalRange::Discrete(FrameInterval::new(0, 15))];
        assert_eq!(choose_fps(&degenerate, fps(15)), None);
    }

    #[test]
    fn frame_intervals_become_rounded_fps() {
        let to_fps = |n, d| FrameInterval::new(n, d).to_fps().map(Fps::get);
        assert_eq!(to_fps(1, 15), Some(15));
        assert_eq!(to_fps(1001, 30000), Some(30));
        assert_eq!(to_fps(2, 15), Some(8));
        assert_eq!(to_fps(0, 15), None);
        assert_eq!(to_fps(2, 1), Some(1));
        assert_eq!(to_fps(3, 1), None);
    }

    #[test]
    fn the_user_camera_negotiates_grbg_324x240() {
        let table = CapabilityTable::grbg_camera();
        for preference in [FormatPreference::Auto, FormatPreference::Bayer] {
            let negotiated = negotiate(&table, &settings(preference, res(640, 480), 15)).unwrap();
            assert_eq!(
                negotiated,
                Negotiated {
                    fourcc: FourCc::GRBG,
                    pixel_format: PixelFormat::Bayer8(BayerPattern::Grbg),
                    resolution: res(324, 240),
                    // Cadences non énumérables : la demande est transmise au pilote.
                    fps: fps(15),
                }
            );
        }
    }

    #[test]
    fn an_impossible_explicit_format_lists_the_offer() {
        let table = CapabilityTable::grbg_camera();
        for preference in [FormatPreference::Mjpeg, FormatPreference::Yuyv] {
            let error = negotiate(&table, &settings(preference, res(640, 480), 15)).unwrap_err();
            match &error {
                CaptureError::UnsupportedFormat {
                    preference: refused,
                    offered,
                } => {
                    assert_eq!(*refused, preference);
                    assert_eq!(offered, "GRBG 162x120, 324x240");
                }
                other => panic!("unexpected {other:?}"),
            }
            assert_eq!(error.recovery(), crate::domain::Recovery::Fatal);
        }
    }

    #[test]
    fn mjpeg_wins_and_unknown_formats_are_listed() {
        let table = CapabilityTable::new()
            .with_format(FourCc::new(*b"H264"), discrete(&[(1920, 1080)]), None)
            .with_format(FourCc::YUYV, discrete(&[(640, 480)]), Some(rates(&[30])))
            .with_format(
                FourCc::MJPG,
                discrete(&[(1280, 720), (640, 480)]),
                Some(rates(&[30, 15])),
            );
        let negotiated =
            negotiate(&table, &settings(FormatPreference::Auto, res(640, 480), 15)).unwrap();
        assert_eq!(negotiated.pixel_format, PixelFormat::Mjpeg);
        assert_eq!(negotiated.resolution, res(640, 480));
        assert_eq!(negotiated.fps, fps(15));
        let error = negotiate(
            &table,
            &settings(FormatPreference::Bayer, res(640, 480), 15),
        )
        .unwrap_err();
        assert!(
            error.to_string().ends_with(
                "camera offers: H264 (unsupported) 1920x1080; YUYV 640x480; MJPG 1280x720, 640x480"
            ),
            "{error}"
        );
        // Aucun format énuméré (webcam débranchée pendant l'ouverture) : reprise, pas
        // d'arrêt de l'application.
        let empty = negotiate(
            &CapabilityTable::new(),
            &settings(FormatPreference::Auto, res(640, 480), 15),
        )
        .unwrap_err();
        assert!(matches!(empty, CaptureError::Io(_)), "{empty:?}");
        assert_eq!(empty.recovery(), crate::domain::Recovery::Reopen);
        assert_eq!(describe(&[]), "no format");
    }

    /// Pilote qui se déconnecte pendant l'énumération des tailles.
    struct Unplugged;

    impl CapabilityQuery for Unplugged {
        fn formats(&self) -> Result<Vec<FourCc>, CaptureError> {
            Ok(vec![FourCc::YUYV])
        }
        fn sizes(&self, _: FourCc) -> Result<Vec<SizeRange>, CaptureError> {
            Err(CaptureError::Disconnected)
        }
        fn intervals(&self, _: FourCc, _: Resolution) -> Result<Vec<IntervalRange>, CaptureError> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn a_disconnection_during_enumeration_is_reported() {
        let error = negotiate(
            &Unplugged,
            &settings(FormatPreference::Auto, res(640, 480), 15),
        )
        .unwrap_err();
        assert!(matches!(error, CaptureError::Disconnected));
    }

    #[test]
    fn missing_size_enumeration_keeps_the_request() {
        let table = CapabilityTable::new().with_format(FourCc::YUYV, Vec::new(), None);
        let negotiated =
            negotiate(&table, &settings(FormatPreference::Auto, res(640, 480), 10)).unwrap();
        assert_eq!(negotiated.resolution, res(640, 480));
        assert_eq!(negotiated.fps, fps(10));
    }
}
