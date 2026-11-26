//! Vérification minimale d'une trame JPEG avant diffusion.
//!
//! Une trame MJPEG d'une webcam UVC peut être tronquée (erreur USB) ou complétée par
//! des octets nuls de bourrage. On vérifie seulement les marqueurs de début (`FF D8`)
//! et de fin (`FF D9`) — sans décoder — et on retire le bourrage, pour ne jamais
//! envoyer au navigateur une image incomplète ni des octets inutiles.

/// Marqueur de début d'image (`SOI`).
const SOI: [u8; 2] = [0xFF, 0xD8];
/// Marqueur de fin d'image (`EOI`).
const EOI: [u8; 2] = [0xFF, 0xD9];

/// Renvoie la longueur utile d'une trame JPEG complète (bourrage final retiré), ou
/// `None` si la trame est vide, ne commence pas par `SOI` ou ne finit pas par `EOI`.
pub fn complete_jpeg_len(data: &[u8]) -> Option<usize> {
    if !data.starts_with(&SOI) {
        return None;
    }
    // Le bourrage éventuel se limite à quelques octets nuls en fin de tampon.
    let end = data.iter().rposition(|&byte| byte != 0)? + 1;
    let trimmed = &data[..end];
    (trimmed.len() >= SOI.len() + EOI.len() && trimmed.ends_with(&EOI)).then_some(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &[u8] = &[0xFF, 0xD8, 0xFF, 0xDA, 0x12, 0xFF, 0xD9];

    #[test]
    fn accepts_a_complete_frame() {
        assert_eq!(complete_jpeg_len(MINIMAL), Some(MINIMAL.len()));
    }

    #[test]
    fn strips_zero_padding() {
        let mut padded = MINIMAL.to_vec();
        padded.extend_from_slice(&[0; 9]);
        assert_eq!(complete_jpeg_len(&padded), Some(MINIMAL.len()));
    }

    #[test]
    fn rejects_truncated_empty_or_foreign_data() {
        assert_eq!(complete_jpeg_len(&MINIMAL[..MINIMAL.len() - 1]), None);
        assert_eq!(complete_jpeg_len(&[]), None);
        assert_eq!(complete_jpeg_len(&[0; 16]), None);
        assert_eq!(complete_jpeg_len(&[0xFF, 0xD8, 0xD9]), None);
        // SOI et EOI qui se chevauchent : pas une image.
        assert_eq!(complete_jpeg_len(&[0xFF, 0xD8, 0xFF, 0xD9][1..]), None);
        assert_eq!(complete_jpeg_len(b"not a jpeg"), None);
    }
}
