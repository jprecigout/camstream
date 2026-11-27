//! Conversion YUYV (YUV 4:2:2 entrelacé `Y0 U Y1 V`) ↔ RGB 24 bits.
//!
//! Coefficients BT.601 en « plage limitée » (Y de 16 à 235, chrominance de 16 à 240),
//! convention des webcams UVC, en arithmétique entière à virgule fixe (8 bits) : aucune
//! opération flottante par pixel.

/// Ramène un résultat intermédiaire dans `0..=255`.
#[inline]
fn clamp_u8(value: i32) -> u8 {
    // Borné à 0..=255 juste avant : la conversion est exacte.
    value.clamp(0, 255) as u8
}

/// Convertit `raw` (lignes de `stride` octets, `width` × `height` pixels) en RGB 24 bits
/// entrelacé dans `rgb` (`width * height * 3` octets).
///
/// Un groupe `Y0 U Y1 V` de 4 octets décrit deux pixels qui partagent la chrominance ;
/// pour une largeur impaire, le dernier groupe ne sert qu'à un pixel.
///
/// Préconditions, garanties par `RawLayout` : `stride ≥ ceil(width / 2) * 4`,
/// `raw.len() ≥ stride * (height - 1) + ceil(width / 2) * 4`,
/// `rgb.len() ≥ width * height * 3`. Hors préconditions, la fonction panique sur un
/// indice hors bornes (jamais d'accès mémoire invalide).
pub(crate) fn yuyv_to_rgb(raw: &[u8], stride: usize, width: usize, height: usize, rgb: &mut [u8]) {
    let row_bytes = width.div_ceil(2) * 4;
    for (y, out) in rgb.chunks_exact_mut(width * 3).take(height).enumerate() {
        let row = &raw[y * stride..y * stride + row_bytes];
        for (pixels, group) in out.chunks_mut(6).zip(row.chunks_exact(4)) {
            let (u, v) = (i32::from(group[1]) - 128, i32::from(group[3]) - 128);
            // Contributions de chrominance communes aux deux pixels du groupe.
            let red = 409 * v + 128;
            let green = -100 * u - 208 * v + 128;
            let blue = 516 * u + 128;
            for (pixel, luma) in pixels.chunks_exact_mut(3).zip([group[0], group[2]]) {
                let luma = 298 * (i32::from(luma) - 16);
                pixel[0] = clamp_u8((luma + red) >> 8);
                pixel[1] = clamp_u8((luma + green) >> 8);
                pixel[2] = clamp_u8((luma + blue) >> 8);
            }
        }
    }
}

/// Conversion inverse RGB → YUYV (chrominance moyennée sur chaque paire de pixels),
/// utilisée par la source synthétique YUYV et par les tests. `raw` reçoit
/// `ceil(width / 2) * 4 * height` octets ; `width` doit être pair ou le dernier pixel
/// de chaque ligne est dupliqué.
pub(crate) fn rgb_to_yuyv(rgb: &[u8], width: usize, height: usize, raw: &mut [u8]) {
    let row_bytes = width.div_ceil(2) * 4;
    for (row_rgb, row_raw) in rgb
        .chunks_exact(width * 3)
        .zip(raw.chunks_exact_mut(row_bytes))
        .take(height)
    {
        for (pair, group) in row_rgb.chunks(6).zip(row_raw.chunks_exact_mut(4)) {
            let first = &pair[..3];
            let second = pair.get(3..6).unwrap_or(first);
            let [y0, u0, v0] = to_yuv(first);
            let [y1, u1, v1] = to_yuv(second);
            group.copy_from_slice(&[
                y0,
                // Moyenne de deux `u8` : tient dans un `u8`.
                (u16::from(u0) + u16::from(u1)).div_ceil(2) as u8,
                y1,
                (u16::from(v0) + u16::from(v1)).div_ceil(2) as u8,
            ]);
        }
    }
}

/// Un pixel RGB en `[Y, U, V]` BT.601 plage limitée.
fn to_yuv(pixel: &[u8]) -> [u8; 3] {
    let (r, g, b) = (
        i32::from(pixel[0]),
        i32::from(pixel[1]),
        i32::from(pixel[2]),
    );
    [
        clamp_u8(((66 * r + 129 * g + 25 * b + 128) >> 8) + 16),
        clamp_u8(((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128),
        clamp_u8(((112 * r - 94 * g - 18 * b + 128) >> 8) + 128),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(raw: &[u8], stride: usize, width: usize, height: usize) -> Vec<u8> {
        let mut rgb = vec![0; width * height * 3];
        yuyv_to_rgb(raw, stride, width, height, &mut rgb);
        rgb
    }

    #[test]
    fn reference_levels_are_converted() {
        // Noir (16), blanc (235) et gris moyen, chrominance neutre (128).
        let raw = [16, 128, 235, 128, 126, 128, 126, 128];
        assert_eq!(
            convert(&raw, 8, 4, 1),
            [0, 0, 0, 255, 255, 255, 128, 128, 128, 128, 128, 128]
        );
        // Rouge BT.601 plage limitée : Y = 81, U = 90, V = 240.
        let red = convert(&[81, 90, 81, 240], 4, 2, 1);
        for pixel in red.chunks_exact(3) {
            assert!(
                pixel[0] >= 250 && pixel[1] <= 5 && pixel[2] <= 5,
                "{pixel:?}"
            );
        }
    }

    #[test]
    fn out_of_range_values_saturate() {
        assert_eq!(
            convert(&[0, 128, 255, 128], 4, 2, 1),
            [0, 0, 0, 255, 255, 255]
        );
        let extreme = convert(&[255, 255, 255, 255], 4, 2, 1);
        assert_eq!(&extreme[..3], &[255, 125, 255]);
    }

    #[test]
    fn round_trip_is_close_for_saturated_colors() {
        let colors: [[u8; 3]; 6] = [
            [255, 0, 0],
            [0, 255, 0],
            [0, 0, 255],
            [255, 255, 0],
            [255, 255, 255],
            [40, 40, 40],
        ];
        for color in colors {
            let rgb = color.repeat(4);
            let mut raw = vec![0; 8];
            rgb_to_yuyv(&rgb, 4, 1, &mut raw);
            let back = convert(&raw, 8, 4, 1);
            for (a, b) in back.iter().zip(&rgb) {
                assert!(a.abs_diff(*b) <= 3, "{color:?} -> {back:?}");
            }
        }
    }

    #[test]
    fn odd_width_and_stride_padding_are_handled() {
        // 3 pixels de large : deux groupes, le second n'alimente qu'un pixel ; 4 octets
        // de bourrage par ligne (stride 12).
        let line = [235, 128, 235, 128, 16, 128, 99, 128, 0xEE, 0xEE, 0xEE, 0xEE];
        let mut raw = line.repeat(2);
        raw.truncate(12 + 8);
        let rgb = convert(&raw, 12, 3, 2);
        let row = [255, 255, 255, 255, 255, 255, 0, 0, 0];
        assert_eq!(rgb, [row, row].concat());
    }
}
