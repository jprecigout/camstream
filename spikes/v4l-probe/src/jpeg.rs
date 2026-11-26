//! Inspection minimale des trames JPEG produites par la webcam.
//!
//! Objectif du spike : vérifier que la taille utile (`bytesused`) délimite bien une
//! image JPEG complète, et détecter les trames MJPEG **sans table de Huffman** (`DHT`),
//! fréquentes sur les webcams UVC : la plupart des navigateurs les affichent (tables
//! par défaut de libjpeg-turbo), mais certains visualiseurs les refusent.

use serde::Serialize;

/// Marqueur de début d'image.
const SOI: u8 = 0xD8;
/// Marqueur de fin d'image.
const EOI: u8 = 0xD9;
/// Marqueur de début des données compressées : fin des en-têtes.
const SOS: u8 = 0xDA;
/// Marqueur de table de Huffman.
const DHT: u8 = 0xC4;

/// Résultat de l'inspection d'une trame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct JpegInfo {
    /// Commence par `FF D8`.
    pub has_soi: bool,
    /// Se termine par `FF D9` (en ignorant un éventuel bourrage d'octets nuls).
    pub has_eoi: bool,
    /// Au moins un segment `DHT` avant `SOS`.
    pub has_dht: bool,
    /// Octets nuls de bourrage après `FF D9`.
    pub trailing_padding: usize,
}

impl JpegInfo {
    /// Trame exploitable par un navigateur : début et fin d'image présents.
    pub fn is_complete(&self) -> bool {
        self.has_soi && self.has_eoi
    }
}

/// Inspecte une trame déjà tronquée à sa taille utile.
pub fn inspect(data: &[u8]) -> JpegInfo {
    let has_soi = data.len() >= 2 && data[0] == 0xFF && data[1] == SOI;
    // Certaines webcams complètent la trame par des zéros : on les ignore pour
    // chercher la fin d'image, et on les compte pour le rapport.
    let trimmed_len = data.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let trailing_padding = data.len() - trimmed_len;
    let has_eoi = trimmed_len >= 2 && data[trimmed_len - 2] == 0xFF && data[trimmed_len - 1] == EOI;
    let has_dht = has_soi && scan_headers_for_dht(data);
    JpegInfo {
        has_soi,
        has_eoi,
        has_dht,
        trailing_padding,
    }
}

/// Parcourt les segments d'en-tête (de `SOI` jusqu'à `SOS`) à la recherche d'un `DHT`.
///
/// Parcours défensif : toute incohérence (longueur hors limites, octet inattendu)
/// arrête l'analyse sans paniquer.
fn scan_headers_for_dht(data: &[u8]) -> bool {
    let mut pos = 2;
    while pos + 1 < data.len() {
        if data[pos] != 0xFF {
            return false;
        }
        // Les octets 0xFF répétés sont du remplissage autorisé avant un marqueur.
        let marker = data[pos + 1];
        if marker == 0xFF {
            pos += 1;
            continue;
        }
        match marker {
            DHT => return true,
            SOS | EOI => return false,
            // Marqueurs autonomes (sans champ de longueur).
            0x01 | 0xD0..=0xD7 => pos += 2,
            _ => {
                let Some(len_bytes) = data.get(pos + 2..pos + 4) else {
                    return false;
                };
                let segment_len = usize::from(u16::from_be_bytes([len_bytes[0], len_bytes[1]]));
                if segment_len < 2 {
                    return false;
                }
                pos += 2 + segment_len;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construit un JPEG minimal (structure seulement, non décodable) avec ou sans DHT.
    fn fake_jpeg(with_dht: bool, padding: usize) -> Vec<u8> {
        let mut data = vec![0xFF, SOI];
        // APP0 de longueur 4 (2 octets de longueur + 2 octets de contenu).
        data.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x04, b'J', b'F']);
        if with_dht {
            data.extend_from_slice(&[0xFF, DHT, 0x00, 0x03, 0x00]);
        }
        data.extend_from_slice(&[0xFF, SOS, 0x00, 0x02, 0x12, 0x34, 0xFF, EOI]);
        data.extend(std::iter::repeat_n(0u8, padding));
        data
    }

    #[test]
    fn complete_frame_with_dht() {
        let info = inspect(&fake_jpeg(true, 0));
        assert!(info.is_complete());
        assert!(info.has_dht);
        assert_eq!(info.trailing_padding, 0);
    }

    #[test]
    fn mjpeg_frame_without_dht_is_detected() {
        let info = inspect(&fake_jpeg(false, 0));
        assert!(info.is_complete());
        assert!(!info.has_dht);
    }

    #[test]
    fn zero_padding_after_eoi_is_ignored_and_counted() {
        let info = inspect(&fake_jpeg(true, 7));
        assert!(info.has_eoi);
        assert_eq!(info.trailing_padding, 7);
    }

    #[test]
    fn truncated_frame_has_no_eoi() {
        let mut data = fake_jpeg(true, 0);
        data.truncate(data.len() - 3);
        let info = inspect(&data);
        assert!(info.has_soi);
        assert!(!info.has_eoi);
        assert!(!info.is_complete());
    }

    #[test]
    fn garbage_and_empty_input_do_not_panic() {
        assert_eq!(inspect(&[]), JpegInfo::default());
        let info = inspect(&[0xFF, SOI, 0xFF, 0xE0, 0xFF]);
        assert!(info.has_soi);
        assert!(!info.has_dht);
        assert!(!inspect(&[0x00, 0x01, 0x02]).has_soi);
    }
}
