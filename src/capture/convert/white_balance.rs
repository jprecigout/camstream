//! Balance des blancs automatique « monde gris » pour les capteurs Bayer bruts.
//!
//! Hypothèse : en moyenne, une scène est grise. Les gains rouge et bleu ramènent les
//! moyennes de ces canaux à celle du vert. Coût : une somme sur un pixel sur
//! [`SAMPLE_STEP`] et deux lectures de table par pixel. Les gains sont lissés d'une
//! trame à l'autre pour éviter le scintillement quand la scène change.

/// Un pixel sur `SAMPLE_STEP` sert à l'estimation (précision largement suffisante).
const SAMPLE_STEP: usize = 4;
/// Gain minimal appliqué (évite d'éteindre un canal sur une scène très colorée).
const MIN_GAIN: f32 = 0.25;
/// Gain maximal appliqué (évite d'amplifier le bruit d'un canal presque noir).
const MAX_GAIN: f32 = 4.0;
/// Part de la nouvelle mesure dans le gain lissé (moyenne exponentielle).
const SMOOTHING: f32 = 0.25;

/// État de la balance des blancs « monde gris » : gains lissés et tables de
/// correspondance reconstruites à chaque trame (2 × 256 octets, sans allocation).
#[derive(Debug, Clone)]
pub(crate) struct GrayWorld {
    gains: Option<(f32, f32)>,
    red_lut: [u8; 256],
    blue_lut: [u8; 256],
}

impl GrayWorld {
    /// Balance sans historique : la première trame fixe directement les gains.
    pub(crate) fn new() -> Self {
        Self {
            gains: None,
            red_lut: identity(),
            blue_lut: identity(),
        }
    }

    /// Gains courants (rouge, bleu), `None` avant la première trame exploitable.
    #[cfg(test)]
    pub(crate) fn gains(&self) -> Option<(f32, f32)> {
        self.gains
    }

    /// Mesure les moyennes de `rgb` (RGB 24 bits entrelacé), met à jour les gains
    /// lissés puis corrige l'image en place. Une image dont un canal est entièrement
    /// noir conserve les gains précédents.
    pub(crate) fn apply(&mut self, rgb: &mut [u8]) {
        let (mut red, mut green, mut blue) = (0_u64, 0_u64, 0_u64);
        for pixel in rgb.chunks_exact(3).step_by(SAMPLE_STEP) {
            red += u64::from(pixel[0]);
            green += u64::from(pixel[1]);
            blue += u64::from(pixel[2]);
        }
        if red > 0 && green > 0 && blue > 0 {
            // Rapports de sommes sur le même nombre de pixels : précision du `f32`
            // suffisante pour un gain.
            let measured = (
                (green as f32 / red as f32).clamp(MIN_GAIN, MAX_GAIN),
                (green as f32 / blue as f32).clamp(MIN_GAIN, MAX_GAIN),
            );
            let gains = match self.gains {
                None => measured,
                Some((r, b)) => (
                    r + (measured.0 - r) * SMOOTHING,
                    b + (measured.1 - b) * SMOOTHING,
                ),
            };
            self.gains = Some(gains);
            fill_lut(&mut self.red_lut, gains.0);
            fill_lut(&mut self.blue_lut, gains.1);
        }
        for pixel in rgb.chunks_exact_mut(3) {
            pixel[0] = self.red_lut[usize::from(pixel[0])];
            pixel[2] = self.blue_lut[usize::from(pixel[2])];
        }
    }
}

/// Table identité (aucune correction).
fn identity() -> [u8; 256] {
    // `index` < 256 : la conversion est exacte.
    std::array::from_fn(|index| index as u8)
}

/// Table `v → min(255, round(v × gain))`.
fn fill_lut(lut: &mut [u8; 256], gain: f32) {
    for (index, entry) in lut.iter_mut().enumerate() {
        // Valeur finie bornée à 0..=255 juste avant la conversion.
        *entry = (index as f32 * gain).round().clamp(0.0, 255.0) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tinted_gray_becomes_neutral() {
        let mut balance = GrayWorld::new();
        let mut rgb = [100_u8, 50, 200].repeat(64);
        balance.apply(&mut rgb);
        assert!(
            rgb.chunks_exact(3).all(|px| px == [50, 50, 50]),
            "{:?}",
            &rgb[..3]
        );
        assert_eq!(balance.gains(), Some((0.5, 0.25)));
    }

    #[test]
    fn gains_are_clamped_and_smoothed() {
        let mut balance = GrayWorld::new();
        // Vert seul très supérieur : gains bornés à 4.
        let mut rgb = [10_u8, 200, 10].repeat(16);
        balance.apply(&mut rgb);
        assert_eq!(balance.gains(), Some((4.0, 4.0)));
        assert_eq!(&rgb[..3], &[40, 200, 40]);
        // Scène neutre ensuite : le gain se rapproche de 1 sans y sauter.
        let mut neutral = [90_u8, 90, 90].repeat(16);
        balance.apply(&mut neutral);
        let (red, blue) = balance.gains().unwrap();
        assert!(
            (red - 3.25).abs() < 1e-6 && (blue - 3.25).abs() < 1e-6,
            "{red} {blue}"
        );
    }

    #[test]
    fn a_black_channel_keeps_the_previous_gains() {
        let mut balance = GrayWorld::new();
        let mut black_red = [0_u8, 100, 100].repeat(8);
        balance.apply(&mut black_red);
        assert_eq!(balance.gains(), None);
        assert_eq!(&black_red[..3], &[0, 100, 100]);
    }
}
