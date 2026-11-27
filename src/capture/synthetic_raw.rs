//! Source synthétique **brute** : mire produite en Bayer `GRBG` ou en YUYV puis
//! convertie en JPEG par la même chaîne que la webcam (ADR 0008).
//!
//! Sélectionnée par `CAMSTREAM_SOURCE=synthetic` avec `CAMSTREAM_FORMAT=bayer` ou
//! `yuyv`. Elle négocie format, résolution et cadence sur une table de capacités
//! simulée — en Bayer, exactement celle de la caméra de l'utilisateur (`GRBG`, 162x120
//! et 324x240) — puis applique le [`FrameEncoder`] choisi par [`encoder_for`] : la
//! chaîne complète négociation → conversion → diffusion HTTP est ainsi testable et
//! démontrable sans matériel, y compris sur le Pi pour mesurer le coût de conversion.

use super::convert::{encoder_for, mosaic, rgb_to_yuyv, FrameEncoder};
use super::negotiation::{
    negotiate, CapabilityTable, FourCc, FrameInterval, IntervalRange, SizeRange,
};
use super::synthetic::{Pacer, SYNTHETIC_SOURCE_NAME};
use crate::domain::{
    BayerPattern, CaptureError, CaptureSettings, FormatPreference, FpsOrigin, Frame, FrameSource,
    PixelFormat, Resolution, SequenceNumber, SourceDescription,
};

/// Couleurs des barres verticales de la mire (blanc, jaune, cyan, vert, magenta,
/// rouge, bleu, noir) : moyennes des trois canaux égales, la balance des blancs
/// « monde gris » laisse donc la mire intacte.
const BARS: [[u8; 3]; 8] = [
    [235, 235, 235],
    [235, 235, 16],
    [16, 235, 235],
    [16, 235, 16],
    [235, 16, 235],
    [235, 16, 16],
    [16, 16, 235],
    [16, 16, 16],
];

/// Nombre de positions du carré mobile sur la largeur de l'image.
const SQUARE_STEPS: usize = 20;

/// Table de capacités simulée pour la préférence de format.
///
/// `Bayer` : la caméra de l'utilisateur ; `Yuyv` : une webcam UVC YUYV typique.
fn simulated_capabilities(preference: FormatPreference) -> CapabilityTable {
    match preference {
        FormatPreference::Yuyv => {
            let size = |w, h| Resolution::new(w, h).map(SizeRange::Discrete);
            let rates = [30, 15, 10]
                .map(|rate| IntervalRange::Discrete(FrameInterval::new(1, rate)))
                .to_vec();
            CapabilityTable::new().with_format(
                FourCc::YUYV,
                [size(320, 240), size(640, 480)].into_iter().flatten(),
                Some(rates),
            )
        }
        _ => CapabilityTable::grbg_camera(),
    }
}

/// Source synthétique brute (stratégie du pattern **Strategy**, port `FrameSource`).
///
/// Chaque trame : mire RGB (barres et carré mobile) → mosaïque Bayer ou YUYV (ce que
/// produirait le capteur) → [`FrameEncoder`] → JPEG. Tous les tampons sont alloués à
/// la construction et réutilisés.
pub struct SyntheticRawSource {
    description: SourceDescription,
    pixel_format: PixelFormat,
    width: usize,
    height: usize,
    background: Vec<u8>,
    scene: Vec<u8>,
    raw: Vec<u8>,
    encoder: Box<dyn FrameEncoder>,
    pacer: Pacer,
    frame_index: usize,
    sequence: SequenceNumber,
}

impl SyntheticRawSource {
    /// Négocie sur la table simulée correspondant à `settings.format` (`Bayer` ou
    /// `Yuyv` ; toute autre préférence est traitée comme `Bayer`) et prépare la chaîne
    /// de conversion.
    pub fn open(settings: &CaptureSettings) -> Result<Self, CaptureError> {
        let table = simulated_capabilities(settings.format);
        let preference = match settings.format {
            FormatPreference::Yuyv => FormatPreference::Yuyv,
            _ => FormatPreference::Bayer,
        };
        let negotiated = negotiate(
            &table,
            &CaptureSettings {
                format: preference,
                ..settings.clone()
            },
        )?;
        let resolution = negotiated.resolution;
        let encoder = encoder_for(negotiated.pixel_format, resolution, None, settings.encoding)?;
        // Résolutions de la table simulée : quelques centaines de pixels de côté.
        let (width, height) = (resolution.width() as usize, resolution.height() as usize);
        let raw_len = match negotiated.pixel_format {
            PixelFormat::Yuyv => width.div_ceil(2) * 4 * height,
            _ => width * height,
        };
        let background: Vec<u8> = (0..width * height)
            .flat_map(|index| BARS[(index % width) * BARS.len() / width])
            .collect();
        Ok(Self {
            description: SourceDescription {
                name: SYNTHETIC_SOURCE_NAME.to_owned(),
                pixel_format: negotiated.pixel_format,
                resolution,
                fps: negotiated.fps,
                // La mire produit elle-même la cadence négociée.
                fps_origin: FpsOrigin::Confirmed,
                encoding: encoder.encoding(),
            },
            pixel_format: negotiated.pixel_format,
            width,
            height,
            scene: background.clone(),
            background,
            raw: vec![0; raw_len],
            encoder,
            pacer: Pacer::new(negotiated.fps),
            frame_index: 0,
            sequence: SequenceNumber::default(),
        })
    }

    /// Dessine la mire de la trame courante dans `scene` puis la convertit en données
    /// brutes dans `raw`.
    fn render(&mut self) {
        self.scene.copy_from_slice(&self.background);
        // Carré gris clair d'un quart de la hauteur, qui traverse l'image.
        let side = (self.height / 4).max(1);
        let step = self.frame_index % SQUARE_STEPS;
        let left = (self.width.saturating_sub(side)) * step / (SQUARE_STEPS - 1);
        let top = (self.height - side) / 2;
        for y in top..top + side {
            let start = (y * self.width + left) * 3;
            let end = (y * self.width + (left + side).min(self.width)) * 3;
            self.scene[start..end].fill(200);
        }
        match self.pixel_format {
            PixelFormat::Yuyv => rgb_to_yuyv(&self.scene, self.width, self.height, &mut self.raw),
            PixelFormat::Bayer8(pattern) => mosaic(&self.scene, self.width, pattern, &mut self.raw),
            // Jamais négocié sur les tables simulées : mosaïque GRBG par défaut.
            PixelFormat::Mjpeg => {
                mosaic(&self.scene, self.width, BayerPattern::Grbg, &mut self.raw)
            }
        }
    }
}

impl FrameSource for SyntheticRawSource {
    /// Attend l'échéance de la trame suivante puis produit, convertit et renvoie la
    /// mire (bloquant, dans le thread de capture).
    fn next_frame(&mut self) -> Result<Frame, CaptureError> {
        self.pacer.wait();
        self.render();
        // Comme pour la webcam : instant de capture pris avant la conversion.
        let captured_at = std::time::Instant::now();
        let data = self.encoder.encode(&self.raw)?;
        self.frame_index = self.frame_index.wrapping_add(1);
        let frame = Frame::captured(
            data,
            self.description.resolution,
            self.sequence,
            captured_at,
        );
        self.sequence = self.sequence.next();
        Ok(frame)
    }

    fn description(&self) -> SourceDescription {
        self.description.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::domain::{
        DevicePath, EncodingSettings, Fps, FrameEncoding, JpegQuality, SourceKind,
    };

    fn settings(format: FormatPreference) -> CaptureSettings {
        CaptureSettings {
            kind: SourceKind::Synthetic,
            device: DevicePath::new("/dev/null"),
            resolution: Resolution::new(640, 480).unwrap(),
            fps: Fps::new(1000).unwrap(),
            format,
            encoding: EncodingSettings::default(),
            read_timeout: Duration::from_secs(1),
        }
    }

    fn decode(jpeg: &[u8]) -> (u16, u16, Vec<u8>) {
        let mut decoder = jpeg_decoder::Decoder::new(jpeg);
        let pixels = decoder.decode().expect("valid JPEG");
        let info = decoder.info().expect("header");
        (info.width, info.height, pixels)
    }

    #[test]
    fn bayer_pattern_emulates_the_user_camera() {
        let mut source = SyntheticRawSource::open(&settings(FormatPreference::Bayer)).unwrap();
        let description = source.description();
        assert_eq!(description.pixel_format.as_str(), "GRBG");
        assert_eq!(description.resolution.to_string(), "324x240");
        assert_eq!(
            description.encoding,
            FrameEncoding::Software {
                quality: JpegQuality::DEFAULT
            }
        );
        let first = source.next_frame().unwrap();
        let second = source.next_frame().unwrap();
        assert_eq!(first.sequence().get() + 1, second.sequence().get());
        // Le carré bouge : deux trames consécutives diffèrent.
        assert_ne!(first.data(), second.data());
        let (width, height, pixels) = decode(first.data());
        assert_eq!((width, height), (324, 240));
        // Barre rouge (6e sur 8) en haut de l'image : rouge dominant après la chaîne
        // mosaïque → débayerisation → balance des blancs → JPEG.
        let x = 324 * 5 / 8 + 324 / 16;
        let pixel = &pixels[(10 * 324 + x) * 3..][..3];
        assert!(
            pixel[0] > 180 && pixel[1] < 70 && pixel[2] < 70,
            "{pixel:?}"
        );
    }

    #[test]
    fn yuyv_pattern_negotiates_640x480() {
        let mut source = SyntheticRawSource::open(&settings(FormatPreference::Yuyv)).unwrap();
        let description = source.description();
        assert_eq!(description.pixel_format, PixelFormat::Yuyv);
        assert_eq!(description.resolution.to_string(), "640x480");
        // 1000 i/s demandés, 30 proposés au plus.
        assert_eq!(description.fps.get(), 30);
        let (width, height, pixels) = decode(source.next_frame().unwrap().data());
        assert_eq!((width, height), (640, 480));
        // Barre bleue (7e sur 8).
        let x = 640 * 6 / 8 + 640 / 16;
        let pixel = &pixels[(10 * 640 + x) * 3..][..3];
        assert!(
            pixel[2] > 180 && pixel[0] < 70 && pixel[1] < 70,
            "{pixel:?}"
        );
    }

    #[test]
    fn small_requests_pick_the_smallest_size() {
        let mut request = settings(FormatPreference::Bayer);
        request.resolution = Resolution::new(200, 150).unwrap();
        let source = SyntheticRawSource::open(&request).unwrap();
        assert_eq!(source.description().resolution.to_string(), "162x120");
    }
}
