//! Débayerisation 8 bits par interpolation bilinéaire, pour les quatre motifs.
//!
//! Chaque photosite ne mesure qu'une couleur ; les deux autres sont la moyenne des
//! voisins les plus proches de cette couleur (4 voisins en croix ou en diagonale, ou
//! 2 voisins horizontaux ou verticaux). Choix justifié dans l'ADR 0008 : à pleine
//! résolution (contrairement au « superpixel » 2x2 qui divise la résolution par deux),
//! sans multiplication, quelques additions par pixel — négligeable devant l'encodage
//! JPEG, même sur Raspberry Pi.
//!
//! Bords : les voisins hors image sont pris par **réflexion** (`-1 → 1`, `n → n-2`),
//! qui conserve la parité donc la couleur du voisin (une simple répétition du bord
//! mélangerait les couleurs). Fonctionne pour toute taille ≥ 2x2, paire ou impaire.

use crate::domain::BayerPattern;

/// Couleur du filtre posé sur un photosite Bayer.
///
/// Détail de l'algorithme de débayerisation : le domaine ne connaît que le motif
/// ([`BayerPattern`], vocabulaire), pas la géométrie des photosites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BayerColor {
    /// Rouge.
    Red,
    /// Vert.
    Green,
    /// Bleu.
    Blue,
}

/// Couleur du photosite en colonne `x`, ligne `y` pour un motif (seule la parité
/// compte : le motif 2x2 se répète).
pub(crate) fn color_at(pattern: BayerPattern, x: usize, y: usize) -> BayerColor {
    use BayerColor::{Blue, Green, Red};
    let cells = match pattern {
        BayerPattern::Bggr => [Blue, Green, Green, Red],
        BayerPattern::Gbrg => [Green, Blue, Red, Green],
        BayerPattern::Grbg => [Green, Red, Blue, Green],
        BayerPattern::Rggb => [Red, Green, Green, Blue],
    };
    cells[(y & 1) * 2 + (x & 1)]
}

/// Voisin précédent par réflexion (`0 → 1`).
#[inline]
fn before(index: usize) -> usize {
    if index == 0 {
        1
    } else {
        index - 1
    }
}

/// Voisin suivant par réflexion (`len - 1 → len - 2`).
#[inline]
fn after(index: usize, len: usize) -> usize {
    if index + 1 == len {
        len - 2
    } else {
        index + 1
    }
}

/// Moyenne arrondie de deux échantillons.
#[inline]
fn mean2(a: u8, b: u8) -> u8 {
    // Somme ≤ 511 : le résultat tient dans un `u8`.
    ((u16::from(a) + u16::from(b) + 1) >> 1) as u8
}

/// Moyenne arrondie de quatre échantillons.
#[inline]
fn mean4(a: u8, b: u8, c: u8, d: u8) -> u8 {
    // Somme ≤ 1022 : le résultat tient dans un `u8`.
    ((u16::from(a) + u16::from(b) + u16::from(c) + u16::from(d) + 2) >> 2) as u8
}

/// Débayerise `raw` (lignes de `stride` octets, `width` × `height` photosites) en
/// RGB 24 bits entrelacé dans `rgb` (`width * height * 3` octets).
///
/// Préconditions, garanties par `RawLayout` (vérifiées par l'appelant) :
/// `width ≥ 2`, `height ≥ 2`, `stride ≥ width`,
/// `raw.len() ≥ stride * (height - 1) + width`, `rgb.len() ≥ width * height * 3`.
/// Hors préconditions, la fonction panique sur un indice hors bornes (jamais d'accès
/// mémoire invalide).
pub(crate) fn demosaic_bilinear(
    raw: &[u8],
    stride: usize,
    width: usize,
    height: usize,
    pattern: BayerPattern,
    rgb: &mut [u8],
) {
    for (y, out) in rgb.chunks_exact_mut(width * 3).take(height).enumerate() {
        let row = |index: usize| &raw[index * stride..index * stride + width];
        let (up, mid, down) = (row(before(y)), row(y), row(after(y, height)));
        let colors = [color_at(pattern, 0, y), color_at(pattern, 1, y)];
        // Sur une ligne qui contient du rouge, les voisins horizontaux d'un vert sont
        // rouges et ses voisins verticaux bleus ; l'inverse sur une ligne bleue.
        let red_row = colors.contains(&BayerColor::Red);
        for (x, pixel) in out.chunks_exact_mut(3).enumerate() {
            let (left, right) = (before(x), after(x, width));
            let value = mid[x];
            let rgb_value = match colors[x & 1] {
                BayerColor::Red => [
                    value,
                    mean4(mid[left], mid[right], up[x], down[x]),
                    mean4(up[left], up[right], down[left], down[right]),
                ],
                BayerColor::Blue => [
                    mean4(up[left], up[right], down[left], down[right]),
                    mean4(mid[left], mid[right], up[x], down[x]),
                    value,
                ],
                BayerColor::Green => {
                    let horizontal = mean2(mid[left], mid[right]);
                    let vertical = mean2(up[x], down[x]);
                    if red_row {
                        [horizontal, value, vertical]
                    } else {
                        [vertical, value, horizontal]
                    }
                }
            };
            pixel.copy_from_slice(&rgb_value);
        }
    }
}

/// Opération inverse (échantillonnage d'une image RGB selon le motif), sans
/// interpolation : produit la mosaïque brute qu'un capteur aurait mesurée.
///
/// Utilisée par la source synthétique Bayer et par les tests. `raw` reçoit
/// `width * height` octets (lignes sans bourrage).
pub(crate) fn mosaic(rgb: &[u8], width: usize, pattern: BayerPattern, raw: &mut [u8]) {
    for (index, (pixel, sample)) in rgb.chunks_exact(3).zip(raw.iter_mut()).enumerate() {
        let (x, y) = (index % width, index / width);
        *sample = match color_at(pattern, x, y) {
            BayerColor::Red => pixel[0],
            BayerColor::Green => pixel[1],
            BayerColor::Blue => pixel[2],
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATTERNS: [BayerPattern; 4] = [
        BayerPattern::Bggr,
        BayerPattern::Gbrg,
        BayerPattern::Grbg,
        BayerPattern::Rggb,
    ];

    #[test]
    fn bayer_patterns_place_colors_by_parity() {
        use BayerColor::{Blue, Green, Red};
        let grid = |pattern: BayerPattern| {
            [(0, 0), (1, 0), (0, 1), (1, 1)].map(|(x, y)| color_at(pattern, x, y))
        };
        assert_eq!(grid(BayerPattern::Grbg), [Green, Red, Blue, Green]);
        assert_eq!(grid(BayerPattern::Rggb), [Red, Green, Green, Blue]);
        assert_eq!(grid(BayerPattern::Bggr), [Blue, Green, Green, Red]);
        assert_eq!(grid(BayerPattern::Gbrg), [Green, Blue, Red, Green]);
        // Le motif se répète tous les deux pixels.
        assert_eq!(color_at(BayerPattern::Grbg, 5, 2), Red);
    }

    fn solid(width: usize, height: usize, color: [u8; 3]) -> Vec<u8> {
        color.repeat(width * height)
    }

    fn demosaic(
        raw: &[u8],
        stride: usize,
        width: usize,
        height: usize,
        p: BayerPattern,
    ) -> Vec<u8> {
        let mut rgb = vec![0; width * height * 3];
        demosaic_bilinear(raw, stride, width, height, p, &mut rgb);
        rgb
    }

    #[test]
    fn a_uniform_color_is_restored_exactly_for_every_pattern_and_size() {
        // Tailles paires, impaires et minimales : bords et coins compris.
        for (width, height) in [(2, 2), (3, 3), (4, 2), (5, 7), (8, 6), (9, 4)] {
            for pattern in PATTERNS {
                for color in [[200, 100, 50], [0, 255, 0], [12, 34, 56]] {
                    let mut raw = vec![0; width * height];
                    mosaic(&solid(width, height, color), width, pattern, &mut raw);
                    let rgb = demosaic(&raw, width, width, height, pattern);
                    assert_eq!(
                        rgb,
                        solid(width, height, color),
                        "{pattern:?} {width}x{height} {color:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn grbg_sites_are_interpolated_from_their_neighbours() {
        // Motif GRBG 4x4, valeurs distinctes par couleur : G = 100, R = 200, B = 40.
        //   G R G R
        //   B G B G
        //   G R G R
        //   B G B G
        let raw: Vec<u8> = (0..16)
            .map(|i| match color_at(BayerPattern::Grbg, i % 4, i / 4) {
                BayerColor::Green => 100,
                BayerColor::Red => 200,
                BayerColor::Blue => 40,
            })
            .collect();
        let rgb = demosaic(&raw, 4, 4, 4, BayerPattern::Grbg);
        // Chaque pixel retrouve exactement les trois niveaux (image sans contours).
        assert!(
            rgb.chunks_exact(3).all(|px| px == [200, 100, 40]),
            "{rgb:?}"
        );
    }

    #[test]
    fn interpolation_averages_the_right_neighbours() {
        // RGGB 4x4, rouge = 0 sauf le photosite rouge (2,2) = 255.
        let mut raw = vec![80_u8; 16];
        for (i, sample) in raw.iter_mut().enumerate() {
            if color_at(BayerPattern::Rggb, i % 4, i / 4) == BayerColor::Red {
                *sample = 0;
            }
        }
        raw[2 * 4 + 2] = 255;
        let rgb = demosaic(&raw, 4, 4, 4, BayerPattern::Rggb);
        let red = |x: usize, y: usize| rgb[(y * 4 + x) * 3];
        assert_eq!(red(2, 2), 255);
        // Vert (3,2) sur ligne rouge : rouge = moyenne horizontale de (2,2) et (2,2)
        // reflété (x = 4 → 2), soit 255.
        assert_eq!(red(3, 2), 255);
        // Vert (1,2) : moyenne de (0,2) = 0 et (2,2) = 255 → 128.
        assert_eq!(red(1, 2), 128);
        // Bleu (1,1) : diagonales (0,0), (2,0), (0,2), (2,2) → 255 / 4 arrondi = 64.
        assert_eq!(red(1, 1), 64);
        // Bleu (3,3) : diagonales (2,2) ×4 par réflexion → 255.
        assert_eq!(red(3, 3), 255);
    }

    #[test]
    fn stride_padding_is_ignored() {
        let (width, height, stride) = (5, 3, 8);
        let mut tight = vec![0; width * height];
        mosaic(
            &solid(width, height, [10, 20, 30]),
            width,
            BayerPattern::Bggr,
            &mut tight,
        );
        let mut padded = vec![0xEE; stride * (height - 1) + width];
        for y in 0..height {
            padded[y * stride..y * stride + width]
                .copy_from_slice(&tight[y * width..(y + 1) * width]);
        }
        assert_eq!(
            demosaic(&padded, stride, width, height, BayerPattern::Bggr),
            solid(width, height, [10, 20, 30])
        );
    }
}
