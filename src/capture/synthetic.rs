//! Source synthétique : mire de test JPEG rejouée à la cadence demandée.
//!
//! Les images (640x480) sont pré-encodées et embarquées dans le binaire par
//! `include_bytes!` (générées par `scripts/generate_synthetic_frames.py`) : aucun
//! encodage à l'exécution. Utilisable sur le Pi pour valider le déploiement sans
//! webcam, en développement et en CI.

use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::domain::{
    CaptureError, Fps, Frame, FrameSource, PixelFormat, Resolution, SequenceNumber,
    SourceDescription,
};

/// Nom de la source, visible dans les logs et `/health`.
pub const SYNTHETIC_SOURCE_NAME: &str = "synthetic test pattern";

/// Résolution des images embarquées.
// Invariant vérifié à la compilation : 640x480 n'a aucune dimension nulle.
const RESOLUTION: Resolution = match Resolution::new(640, 480) {
    Some(resolution) => resolution,
    None => panic!("640x480 is a valid resolution"),
};

/// Inclut une image de la mire (chemin relatif à la racine de la crate).
macro_rules! frame_file {
    ($name:literal) => {
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/synthetic/",
            $name
        ))
    };
}

/// Images de la mire, dans l'ordre de lecture (un carré se déplace de gauche à droite).
static FRAMES: [&[u8]; 10] = [
    frame_file!("frame-00.jpg"),
    frame_file!("frame-01.jpg"),
    frame_file!("frame-02.jpg"),
    frame_file!("frame-03.jpg"),
    frame_file!("frame-04.jpg"),
    frame_file!("frame-05.jpg"),
    frame_file!("frame-06.jpg"),
    frame_file!("frame-07.jpg"),
    frame_file!("frame-08.jpg"),
    frame_file!("frame-09.jpg"),
];

/// Source synthétique : rejoue des images JPEG embarquées à la cadence demandée.
///
/// Stratégie du pattern **Strategy** (implémente `FrameSource`). La résolution
/// demandée est ignorée (images fixes en 640x480), comme un pilote qui impose la
/// sienne ; [`FrameSource::description`] indique la valeur effective. Les trames
/// sont des `Bytes` statiques : aucune copie, aucune allocation par trame.
#[derive(Debug)]
pub struct SyntheticSource {
    fps: Fps,
    interval: Duration,
    resolution: Resolution,
    next_index: usize,
    sequence: SequenceNumber,
    next_deadline: Instant,
}

impl SyntheticSource {
    /// Crée une source qui produit `fps` images par seconde ; la première trame est
    /// disponible immédiatement.
    pub fn new(fps: Fps) -> Self {
        Self {
            fps,
            interval: fps.frame_interval(),
            resolution: RESOLUTION,
            next_index: 0,
            sequence: SequenceNumber::default(),
            next_deadline: Instant::now(),
        }
    }
}

impl FrameSource for SyntheticSource {
    /// Attend l'échéance de la trame suivante (bloquant, une période au plus) puis
    /// renvoie l'image suivante de la mire.
    fn next_frame(&mut self) -> Result<Frame, CaptureError> {
        let now = Instant::now();
        if let Some(wait) = self.next_deadline.checked_duration_since(now) {
            std::thread::sleep(wait);
        }
        // Cadence régulière sans dérive ; après un retard (thread suspendu), on
        // repart de maintenant au lieu de rattraper par une rafale de trames.
        self.next_deadline = (self.next_deadline + self.interval).max(Instant::now());

        let data = Bytes::from_static(FRAMES[self.next_index]);
        self.next_index = (self.next_index + 1) % FRAMES.len();
        let frame = Frame::new(data, self.resolution, self.sequence);
        self.sequence = self.sequence.next();
        Ok(frame)
    }

    fn description(&self) -> SourceDescription {
        SourceDescription {
            name: SYNTHETIC_SOURCE_NAME.to_owned(),
            pixel_format: PixelFormat::Mjpeg,
            resolution: self.resolution,
            fps: self.fps,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::jpeg::complete_jpeg_len;

    #[test]
    fn embedded_frames_are_complete_jpeg_images() {
        for (index, frame) in FRAMES.iter().enumerate() {
            assert_eq!(complete_jpeg_len(frame), Some(frame.len()), "image {index}");
        }
    }

    #[test]
    fn frames_cycle_with_increasing_sequence_numbers() {
        let mut source = SyntheticSource::new(Fps::new(1000).unwrap());
        let frames: Vec<Frame> = (0..FRAMES.len() + 1)
            .map(|_| source.next_frame().unwrap())
            .collect();
        for (expected, frame) in frames.iter().enumerate() {
            assert_eq!(frame.sequence().get(), expected as u64);
        }
        // Après un tour complet, la mire recommence (même zone mémoire statique).
        assert_eq!(
            frames[0].data().as_ptr(),
            frames[FRAMES.len()].data().as_ptr()
        );
        assert_ne!(frames[0].data(), frames[1].data());
    }

    #[test]
    fn respects_the_requested_rate() {
        let mut source = SyntheticSource::new(Fps::new(50).unwrap());
        let started = Instant::now();
        for _ in 0..6 {
            source.next_frame().unwrap();
        }
        // 6 trames à 50 i/s : la première est immédiate, puis 5 intervalles de 20 ms.
        assert!(started.elapsed() >= Duration::from_millis(100));
    }

    #[test]
    fn describes_itself() {
        let source = SyntheticSource::new(Fps::new(15).unwrap());
        let description = source.description();
        assert_eq!(description.name, SYNTHETIC_SOURCE_NAME);
        assert_eq!(description.resolution.to_string(), "640x480");
        assert_eq!(description.fps.get(), 15);
        assert_eq!(description.pixel_format, PixelFormat::Mjpeg);
    }
}
