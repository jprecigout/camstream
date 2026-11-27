//! Transformation du contenu d'un tampon de capture en trame JPEG diffusable
//! (ADR 0008).
//!
//! Le port `FrameSource` garantit au cœur des trames **toujours JPEG** : la conversion
//! des formats bruts est donc un détail des adaptateurs de source, et vit ici, dans
//! `capture`, plutôt que dans `domain` (le cœur n'a pas à connaître l'encodage) ou
//! dans le superviseur (qui ne voit que des `Frame` JPEG). Elle s'exécute dans le
//! thread de capture, dans l'appel bloquant `next_frame`, jamais sur le runtime async.
//!
//! - [`FrameEncoder`] : port interne (pattern **Strategy**) appliqué par
//!   `V4l2Source` et la source synthétique brute à chaque tampon ;
//! - [`JpegPassthrough`] : MJPEG natif, simple vérification (`SOI`/`EOI`) et copie ;
//! - [`RawJpegEncoder`] : chaîne (pattern **Pipeline**) brut → RGB (débayerisation
//!   bilinéaire ou conversion YUYV) → balance des blancs (Bayer) → JPEG
//!   (`jpeg-encoder`, 100 % Rust), avec tampon RGB réutilisé ;
//! - [`encoder_for`] : fabrique (pattern **Factory**) qui choisit la stratégie selon le
//!   format négocié.

mod bayer;
mod white_balance;
mod yuyv;

pub(crate) use bayer::mosaic;
pub(crate) use yuyv::rgb_to_yuyv;

use bytes::Bytes;
use jpeg_encoder::{ColorType, Encoder};

use super::jpeg::complete_jpeg_len;
use crate::domain::{
    BayerPattern, CaptureError, EncodingSettings, FrameEncoding, JpegQuality, PixelFormat,
    Resolution, WhiteBalance,
};
use white_balance::GrayWorld;

/// Conversion du contenu utile d'un tampon de capture en JPEG.
///
/// Implémente le pattern **Strategy** : la source applique la stratégie choisie à la
/// négociation sans connaître le format. Appelé dans le thread de capture ; les
/// implémentations réutilisent leurs tampons de travail d'une trame à l'autre.
pub trait FrameEncoder: Send {
    /// Produit le JPEG correspondant à `payload` (octets utiles du tampon, déjà
    /// tronqués à `bytesused`). Une trame incomplète ou invalide donne
    /// [`CaptureError::CorruptedFrame`] (trame jetée, reprise `Skip`).
    fn encode(&mut self, payload: &[u8]) -> Result<Bytes, CaptureError>;

    /// Origine des JPEG produits, pour `SourceDescription`.
    fn encoding(&self) -> FrameEncoding;
}

/// Stratégie MJPEG natif : la trame est déjà un JPEG ; on vérifie seulement ses
/// marqueurs, on retire le bourrage final et on la copie (seule copie de la chaîne,
/// le tampon mmap étant rendu au pilote à la lecture suivante).
#[derive(Debug, Clone, Copy, Default)]
pub struct JpegPassthrough;

impl FrameEncoder for JpegPassthrough {
    fn encode(&mut self, payload: &[u8]) -> Result<Bytes, CaptureError> {
        let len = complete_jpeg_len(payload).ok_or(CaptureError::CorruptedFrame)?;
        Ok(Bytes::copy_from_slice(&payload[..len]))
    }

    fn encoding(&self) -> FrameEncoding {
        FrameEncoding::Native
    }
}

/// Format brut pris en charge par [`RawJpegEncoder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawFormat {
    /// YUV 4:2:2 entrelacé, 2 octets par pixel.
    Yuyv,
    /// Mosaïque Bayer, 1 octet par photosite.
    Bayer8(BayerPattern),
}

impl RawFormat {
    /// Format brut correspondant, `None` pour un format compressé (MJPEG).
    pub fn from_pixel_format(format: PixelFormat) -> Option<Self> {
        match format {
            PixelFormat::Mjpeg => None,
            PixelFormat::Yuyv => Some(Self::Yuyv),
            PixelFormat::Bayer8(pattern) => Some(Self::Bayer8(pattern)),
        }
    }
}

/// Géométrie d'une trame brute : format, dimensions et pas de ligne (`bytesperline`).
///
/// Implémente le pattern **Newtype** au sens large : un `RawLayout` construit garantit
/// les préconditions des conversions (dimensions minimales, pas de ligne suffisant,
/// limite de 65 535 pixels de l'encodeur JPEG).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawLayout {
    format: RawFormat,
    resolution: Resolution,
    width: usize,
    height: usize,
    stride: usize,
}

impl RawLayout {
    /// Valide la géométrie. `stride` est le pas de ligne annoncé par le pilote ; une
    /// valeur absente, nulle ou trop petite est remplacée par le pas minimal du format.
    ///
    /// Erreur [`CaptureError::UnsupportedResolution`] (fatale) si une dimension dépasse
    /// 65 535 (limite JPEG), si l'image dépasse 4096x4096 pixels (mémoire du Pi) ou si
    /// une mosaïque Bayer fait moins de 2x2.
    pub fn new(
        format: RawFormat,
        resolution: Resolution,
        stride: Option<u32>,
    ) -> Result<Self, CaptureError> {
        let refuse = |reason| CaptureError::UnsupportedResolution { resolution, reason };
        let max = u32::from(u16::MAX);
        if resolution.width() > max || resolution.height() > max {
            return Err(refuse("JPEG images are limited to 65535 pixels per side"));
        }
        if u64::from(resolution.width()) * u64::from(resolution.height()) > MAX_PIXELS {
            return Err(refuse(
                "software conversion is limited to 4096x4096 pixels (16 Mpx)",
            ));
        }
        if matches!(format, RawFormat::Bayer8(_))
            && (resolution.width() < 2 || resolution.height() < 2)
        {
            return Err(refuse("Bayer demosaicing needs at least 2x2 pixels"));
        }
        // Dimensions ≤ 65 535 : conversions en `usize` exactes sur toute cible ≥ 32 bits.
        let (width, height) = (resolution.width() as usize, resolution.height() as usize);
        let row_bytes = Self::row_bytes_of(format, width);
        let stride = stride
            .and_then(|stride| usize::try_from(stride).ok())
            .unwrap_or(0)
            .max(row_bytes);
        Ok(Self {
            format,
            resolution,
            width,
            height,
            stride,
        })
    }

    fn row_bytes_of(format: RawFormat, width: usize) -> usize {
        match format {
            RawFormat::Yuyv => width.div_ceil(2) * 4,
            RawFormat::Bayer8(_) => width,
        }
    }

    /// Octets utiles d'une ligne (hors bourrage).
    pub fn row_bytes(&self) -> usize {
        Self::row_bytes_of(self.format, self.width)
    }

    /// Pas de ligne retenu (octets entre deux débuts de ligne).
    pub fn stride(&self) -> usize {
        self.stride
    }

    /// Taille minimale d'une trame complète : la dernière ligne peut ne pas être
    /// suivie de son bourrage.
    pub fn min_payload(&self) -> usize {
        // Saturant : un pas de ligne aberrant (cible 32 bits) rend la trame impossible à
        // satisfaire (rejetée) au lieu de déborder.
        self.stride
            .saturating_mul(self.height - 1)
            .saturating_add(self.row_bytes())
    }

    /// Format brut.
    pub fn format(&self) -> RawFormat {
        self.format
    }

    /// Résolution de l'image.
    pub fn resolution(&self) -> Resolution {
        self.resolution
    }
}

/// Stratégie « format brut → JPEG » : conversion en RGB, balance des blancs
/// (Bayer, optionnelle) puis encodage JPEG logiciel.
///
/// Implémente les patterns **Strategy** (port [`FrameEncoder`]) et **Pipeline**
/// (étapes successives sur un tampon RGB **réutilisé**). Allocations par trame : le
/// `Vec` du JPEG produit, transféré sans copie dans le `Bytes` de la trame (capacité
/// estimée d'après la trame précédente), et les tampons internes de `jpeg-encoder`.
pub struct RawJpegEncoder {
    layout: RawLayout,
    quality: JpegQuality,
    balance: Option<GrayWorld>,
    rgb: Vec<u8>,
    capacity_hint: usize,
}

/// Nombre maximal de pixels convertis par logiciel (4096x4096) : borne la mémoire du
/// tampon RGB (48 Mio) face à une taille aberrante annoncée par un pilote.
const MAX_PIXELS: u64 = 4096 * 4096;

/// Capacité initiale du tampon JPEG, avant de connaître la taille réelle des trames.
const INITIAL_JPEG_CAPACITY: usize = 16 * 1024;

impl RawJpegEncoder {
    /// Prépare l'encodeur et alloue une fois pour toutes le tampon RGB.
    pub fn new(layout: RawLayout, settings: EncodingSettings) -> Self {
        let balance = match (layout.format, settings.white_balance) {
            (RawFormat::Bayer8(_), WhiteBalance::GrayWorld) => Some(GrayWorld::new()),
            // YUYV : la webcam a déjà fait sa balance des blancs (processeur d'image).
            _ => None,
        };
        Self {
            layout,
            quality: settings.quality,
            balance,
            rgb: vec![0; layout.width * layout.height * 3],
            capacity_hint: INITIAL_JPEG_CAPACITY,
        }
    }

    /// Dernière image RGB convertie (tests de la chaîne).
    #[cfg(test)]
    fn rgb(&self) -> &[u8] {
        &self.rgb
    }
}

/// Pas de `Debug` dérivé : le tampon RGB ferait des centaines de kilooctets de texte.
impl std::fmt::Debug for RawJpegEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawJpegEncoder")
            .field("layout", &self.layout)
            .field("quality", &self.quality)
            .field("white_balance", &self.balance.is_some())
            .finish_non_exhaustive()
    }
}

impl FrameEncoder for RawJpegEncoder {
    fn encode(&mut self, payload: &[u8]) -> Result<Bytes, CaptureError> {
        let layout = self.layout;
        if payload.len() < layout.min_payload() {
            // Trame tronquée (bytesused trop petit) : jamais d'image partiellement lue.
            tracing::debug!(
                received = payload.len(),
                expected = layout.min_payload(),
                "raw frame shorter than expected"
            );
            return Err(CaptureError::CorruptedFrame);
        }
        match layout.format {
            RawFormat::Bayer8(pattern) => bayer::demosaic_bilinear(
                payload,
                layout.stride,
                layout.width,
                layout.height,
                pattern,
                &mut self.rgb,
            ),
            RawFormat::Yuyv => yuyv::yuyv_to_rgb(
                payload,
                layout.stride,
                layout.width,
                layout.height,
                &mut self.rgb,
            ),
        }
        if let Some(balance) = &mut self.balance {
            balance.apply(&mut self.rgb);
        }
        let mut jpeg = Vec::with_capacity(self.capacity_hint);
        // Dimensions validées ≤ 65 535 par `RawLayout::new`.
        let (width, height) = (layout.width as u16, layout.height as u16);
        Encoder::new(&mut jpeg, self.quality.get())
            .encode(&self.rgb, width, height, ColorType::Rgb)
            .map_err(|error| {
                // Impossible avec un tampon et des dimensions validés ; on jette la trame
                // plutôt que d'arrêter la capture.
                tracing::warn!(%error, "JPEG encoding failed");
                CaptureError::CorruptedFrame
            })?;
        // Marge de 25 % pour absorber la variation de taille entre deux trames.
        self.capacity_hint = jpeg.len() + jpeg.len() / 4;
        Ok(Bytes::from(jpeg))
    }

    fn encoding(&self) -> FrameEncoding {
        FrameEncoding::Software {
            quality: self.quality,
        }
    }
}

/// Choisit la stratégie de conversion pour un format négocié (pattern **Factory**) :
/// [`JpegPassthrough`] pour MJPEG, [`RawJpegEncoder`] pour les formats bruts.
///
/// `stride` est le `bytesperline` annoncé par le pilote (`None` : lignes jointives).
pub fn encoder_for(
    format: PixelFormat,
    resolution: Resolution,
    stride: Option<u32>,
    settings: EncodingSettings,
) -> Result<Box<dyn FrameEncoder>, CaptureError> {
    match RawFormat::from_pixel_format(format) {
        None => Ok(Box::new(JpegPassthrough)),
        Some(raw) => {
            let layout = RawLayout::new(raw, resolution, stride)?;
            Ok(Box::new(RawJpegEncoder::new(layout, settings)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Recovery;

    fn res(width: u32, height: u32) -> Resolution {
        Resolution::new(width, height).unwrap()
    }

    fn settings(white_balance: WhiteBalance) -> EncodingSettings {
        EncodingSettings {
            quality: JpegQuality::DEFAULT,
            white_balance,
        }
    }

    /// Décode un JPEG et renvoie (largeur, hauteur, pixels RGB).
    fn decode(jpeg: &[u8]) -> (u16, u16, Vec<u8>) {
        let mut decoder = jpeg_decoder::Decoder::new(jpeg);
        let pixels = decoder.decode().expect("valid JPEG");
        let info = decoder.info().expect("JPEG header");
        assert_eq!(info.pixel_format, jpeg_decoder::PixelFormat::RGB24);
        (info.width, info.height, pixels)
    }

    /// Mosaïque GRBG d'une image coupée en deux : rouge à gauche, bleu à droite.
    fn red_blue_grbg(width: usize, height: usize) -> Vec<u8> {
        let rgb: Vec<u8> = (0..width * height)
            .flat_map(|i| {
                if i % width < width / 2 {
                    [220, 30, 30]
                } else {
                    [30, 30, 220]
                }
            })
            .collect();
        let mut raw = vec![0; width * height];
        mosaic(&rgb, width, BayerPattern::Grbg, &mut raw);
        raw
    }

    #[test]
    fn passthrough_validates_and_trims_mjpeg() {
        let mut encoder = JpegPassthrough;
        let frame = encoder
            .encode(&[0xFF, 0xD8, 0x01, 0xFF, 0xD9, 0, 0])
            .unwrap();
        assert_eq!(&frame[..], &[0xFF, 0xD8, 0x01, 0xFF, 0xD9]);
        assert!(matches!(
            encoder.encode(&[0xFF, 0xD8, 0x01]),
            Err(CaptureError::CorruptedFrame)
        ));
        assert_eq!(encoder.encoding(), FrameEncoding::Native);
    }

    #[test]
    fn grbg_frames_become_valid_jpeg_with_the_right_colors() {
        // Taille de la caméra de l'utilisateur : 324x240 (non multiple de 16).
        let (width, height) = (324, 240);
        let layout =
            RawLayout::new(RawFormat::Bayer8(BayerPattern::Grbg), res(324, 240), None).unwrap();
        let mut encoder = RawJpegEncoder::new(layout, settings(WhiteBalance::Off));
        let jpeg = encoder.encode(&red_blue_grbg(width, height)).unwrap();
        assert!(jpeg.starts_with(&[0xFF, 0xD8]) && jpeg.ends_with(&[0xFF, 0xD9]));
        assert_eq!(complete_jpeg_len(&jpeg), Some(jpeg.len()));
        let (w, h, pixels) = decode(&jpeg);
        assert_eq!((w, h), (324, 240));
        let at = |x: usize, y: usize| &pixels[(y * width + x) * 3..][..3];
        let left = at(40, 120);
        let right = at(280, 120);
        assert!(left[0] > 180 && left[2] < 80, "left {left:?}");
        assert!(right[2] > 180 && right[0] < 80, "right {right:?}");
        assert_eq!(
            encoder.encoding(),
            FrameEncoding::Software {
                quality: JpegQuality::DEFAULT
            }
        );
    }

    #[test]
    fn the_smallest_size_of_the_user_camera_is_encoded() {
        // 162x120 : largeur multiple de 2 mais ni de 4, ni de 8, ni de 16 (blocs JPEG).
        let (width, height) = (162, 120);
        let layout =
            RawLayout::new(RawFormat::Bayer8(BayerPattern::Grbg), res(162, 120), None).unwrap();
        let mut encoder = RawJpegEncoder::new(layout, settings(WhiteBalance::GrayWorld));
        let (w, h, pixels) = decode(&encoder.encode(&red_blue_grbg(width, height)).unwrap());
        assert_eq!((w, h), (162, 120));
        // Colonne la plus à droite (bord reflété) : toujours bleue.
        let edge = &pixels[(60 * width + width - 1) * 3..][..3];
        assert!(edge[2] > edge[0], "{edge:?}");
    }

    #[test]
    fn odd_sizes_and_padded_strides_are_encoded() {
        // 13x7 : impair et non multiple de 8 ; pas de ligne de 16 octets.
        let (width, height, stride) = (13, 7, 16);
        let tight = red_blue_grbg(width, height);
        let mut padded = vec![0xAA; stride * height];
        for y in 0..height {
            padded[y * stride..][..width].copy_from_slice(&tight[y * width..][..width]);
        }
        // Dernière ligne sans bourrage : longueur minimale acceptée.
        padded.truncate(stride * (height - 1) + width);
        let layout =
            RawLayout::new(RawFormat::Bayer8(BayerPattern::Grbg), res(13, 7), Some(16)).unwrap();
        assert_eq!(layout.min_payload(), padded.len());
        let mut encoder = RawJpegEncoder::new(layout, settings(WhiteBalance::GrayWorld));
        let (w, h, _) = decode(&encoder.encode(&padded).unwrap());
        assert_eq!((w, h), (13, 7));
    }

    #[test]
    fn truncated_raw_frames_are_rejected() {
        let layout = RawLayout::new(RawFormat::Yuyv, res(320, 240), None).unwrap();
        assert_eq!(layout.min_payload(), 320 * 240 * 2);
        let mut encoder = RawJpegEncoder::new(layout, settings(WhiteBalance::Off));
        let short = vec![128; layout.min_payload() - 1];
        let error = encoder.encode(&short).unwrap_err();
        assert!(matches!(error, CaptureError::CorruptedFrame));
        assert_eq!(error.recovery(), Recovery::Skip);
        assert!(matches!(
            encoder.encode(&[]),
            Err(CaptureError::CorruptedFrame)
        ));
        // Une trame plus longue que nécessaire (tampon entier) est acceptée.
        let long = vec![128; layout.min_payload() + 100];
        assert!(encoder.encode(&long).is_ok());
    }

    #[test]
    fn malformed_raw_frames_never_panic() {
        // Toutes les combinaisons de petites tailles, de pas de ligne annoncés (absent,
        // trop petit, exact, avec bourrage) et de longueurs de trame (vide, tronquée,
        // exacte, trop longue) : `Ok` ou `CorruptedFrame`, jamais de panique.
        let formats = [
            RawFormat::Yuyv,
            RawFormat::Bayer8(BayerPattern::Grbg),
            RawFormat::Bayer8(BayerPattern::Bggr),
        ];
        for format in formats {
            for (width, height) in [(1, 1), (2, 2), (3, 2), (5, 3), (162, 4)] {
                let Ok(layout) = RawLayout::new(format, res(width, height), None) else {
                    continue;
                };
                for stride in [None, Some(1), Some(layout.row_bytes() as u32), Some(1000)] {
                    let layout = RawLayout::new(format, res(width, height), stride).unwrap();
                    let mut encoder =
                        RawJpegEncoder::new(layout, settings(WhiteBalance::GrayWorld));
                    let needed = layout.min_payload();
                    for len in [0, 1, needed.saturating_sub(1), needed, needed + 7] {
                        let payload = vec![0x5A; len];
                        match encoder.encode(&payload) {
                            Ok(jpeg) => assert!(len >= needed && jpeg.starts_with(&[0xFF, 0xD8])),
                            Err(error) => {
                                assert!(len < needed, "{format:?} {width}x{height} {len}");
                                assert!(matches!(error, CaptureError::CorruptedFrame));
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn yuyv_frames_become_jpeg() {
        let (width, height) = (64, 48);
        let rgb = [0_u8, 200, 0].repeat(width * height);
        let mut raw = vec![0; width * height * 2];
        rgb_to_yuyv(&rgb, width, height, &mut raw);
        let mut encoder = encoder_for(
            PixelFormat::Yuyv,
            res(64, 48),
            None,
            settings(WhiteBalance::GrayWorld),
        )
        .unwrap();
        let (w, h, pixels) = decode(&encoder.encode(&raw).unwrap());
        assert_eq!((w, h), (64, 48));
        let center = &pixels[(24 * width + 32) * 3..][..3];
        // Pas de balance des blancs en YUYV : le vert reste vert.
        assert!(
            center[1] > 170 && center[0] < 40 && center[2] < 40,
            "{center:?}"
        );
    }

    #[test]
    fn white_balance_neutralises_a_tinted_bayer_frame() {
        let (width, height) = (32, 32);
        let tinted = [120_u8, 60, 30].repeat(width * height);
        let mut raw = vec![0; width * height];
        mosaic(&tinted, width, BayerPattern::Rggb, &mut raw);
        let layout =
            RawLayout::new(RawFormat::Bayer8(BayerPattern::Rggb), res(32, 32), None).unwrap();
        let mut encoder = RawJpegEncoder::new(layout, settings(WhiteBalance::GrayWorld));
        encoder.encode(&raw).unwrap();
        assert!(encoder.rgb().chunks_exact(3).all(|px| px == [60, 60, 60]));
    }

    #[test]
    fn layouts_are_validated() {
        let bayer = RawFormat::Bayer8(BayerPattern::Bggr);
        for (resolution, ok) in [
            (res(1, 10), false),
            (res(10, 1), false),
            (res(2, 2), true),
            (res(4096, 4096), true),
            (res(4096, 4097), false),
            (res(65_536, 2), false),
        ] {
            let result = RawLayout::new(bayer, resolution, None);
            assert_eq!(result.is_ok(), ok, "{resolution}");
            if let Err(error) = result {
                assert_eq!(error.recovery(), Recovery::Fatal);
            }
        }
        // Un pixel de large suffit en YUYV.
        assert!(RawLayout::new(RawFormat::Yuyv, res(1, 1), None).is_ok());
        // Pas de ligne trop petit ou nul : remplacé par le minimal.
        let layout = RawLayout::new(RawFormat::Yuyv, res(5, 2), Some(4)).unwrap();
        assert_eq!((layout.row_bytes(), layout.stride()), (12, 12));
        let layout = RawLayout::new(bayer, res(5, 2), Some(0)).unwrap();
        assert_eq!(layout.stride(), 5);
        assert!(matches!(
            encoder_for(
                PixelFormat::Mjpeg,
                res(640, 480),
                None,
                settings(WhiteBalance::Off)
            )
            .unwrap()
            .encoding(),
            FrameEncoding::Native
        ));
    }

    /// Mesure indicative du coût de conversion, rapportée dans l'ADR 0008 :
    /// `cargo test --release --lib -- --ignored --nocapture conversion_cost`.
    #[test]
    #[ignore = "measurement, run explicitly in release mode"]
    fn conversion_cost() {
        let grbg = RawFormat::Bayer8(BayerPattern::Grbg);
        for (format, width, height) in [
            (grbg, 324, 240),
            (grbg, 640, 480),
            (RawFormat::Yuyv, 324, 240),
            (RawFormat::Yuyv, 640, 480),
        ] {
            let layout = RawLayout::new(format, res(width, height), None).unwrap();
            // Pire cas : contenu très texturé (le coût de l'entropie JPEG dépend du
            // contenu ; une scène réelle est plus lisse, donc plus rapide à encoder).
            let raw: Vec<u8> = (0..layout.min_payload())
                .map(|i| ((i % 251) ^ (i / 977)) as u8)
                .collect();
            let mut encoder = RawJpegEncoder::new(layout, settings(WhiteBalance::GrayWorld));
            let mut convert_only = std::time::Duration::ZERO;
            let rounds = 100;
            let started = std::time::Instant::now();
            let mut size = 0;
            for _ in 0..rounds {
                let before = std::time::Instant::now();
                match format {
                    RawFormat::Bayer8(pattern) => bayer::demosaic_bilinear(
                        &raw,
                        layout.stride,
                        layout.width,
                        layout.height,
                        pattern,
                        &mut encoder.rgb,
                    ),
                    RawFormat::Yuyv => yuyv::yuyv_to_rgb(
                        &raw,
                        layout.stride,
                        layout.width,
                        layout.height,
                        &mut encoder.rgb,
                    ),
                }
                convert_only += before.elapsed();
                size = std::hint::black_box(encoder.encode(&raw).unwrap()).len();
            }
            let total = started.elapsed() - convert_only;
            println!(
                "{format:?} {width}x{height}: {:.2} ms/frame total, of which {:.2} ms to RGB; {size} bytes",
                total.as_secs_f64() * 1000.0 / f64::from(rounds),
                convert_only.as_secs_f64() * 1000.0 / f64::from(rounds),
            );
        }
    }
}
