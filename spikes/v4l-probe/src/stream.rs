//! Abstractions de flux de trames utilisées par les scénarios de mesure.
//!
//! Implémente le pattern **Strategy** : les scénarios (`probe`) ne connaissent que ces
//! traits ; l'adaptateur `device` les implémente au-dessus de `v4l`, et les tests
//! fournissent un flux scripté. Ainsi la logique de mesure se teste sans webcam.

use std::io;
use std::time::Duration;

/// Trame brute telle que renvoyée par le pilote.
#[derive(Debug, Clone, Copy)]
pub struct RawFrame<'a> {
    /// Tampon complet (avec `v4l` 0.14 : le tampon mmap **entier**).
    pub buffer: &'a [u8],
    /// Taille utile annoncée par le pilote (`v4l2_buffer.bytesused`).
    pub bytes_used: usize,
    /// Numéro de séquence du pilote.
    pub sequence: u32,
}

impl<'a> RawFrame<'a> {
    /// Données utiles : le tampon tronqué à `bytes_used`.
    ///
    /// Borné à la taille du tampon pour ne jamais paniquer si le pilote annonçait
    /// une taille incohérente.
    pub fn payload(&self) -> &'a [u8] {
        &self.buffer[..self.bytes_used.min(self.buffer.len())]
    }
}

/// Flux de trames bloquant.
pub trait FrameStream {
    /// Fixe le délai maximal d'attente d'une trame.
    fn set_timeout(&mut self, timeout: Duration);
    /// Lit la prochaine trame (bloquant, borné par le délai).
    fn next_frame(&mut self) -> io::Result<RawFrame<'_>>;
}

/// Durées des étapes d'ouverture d'un flux.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenTimings {
    /// Ouverture du nœud (`open`).
    pub open: Duration,
    /// Négociation du format et de la cadence (`S_FMT`, `S_PARM`).
    pub configure: Duration,
    /// Allocation et projection des tampons (`REQBUFS`, `QUERYBUF`, `mmap`).
    pub buffers: Duration,
}

/// Fabrique de flux (pattern **Factory**) : permet de mesurer les cycles
/// d'ouverture/fermeture indépendamment du matériel.
pub trait StreamOpener {
    /// Type de flux produit.
    type Stream: FrameStream;
    /// Type d'erreur d'ouverture.
    type Error: std::error::Error + Send + Sync + 'static;
    /// Ouvre et configure un nouveau flux.
    fn open(&mut self) -> Result<(Self::Stream, OpenTimings), Self::Error>;
}

#[cfg(test)]
pub(crate) mod fake {
    //! Implémentations scriptées pour les tests.

    use super::*;
    use std::collections::VecDeque;

    /// Résultat scripté d'un appel à `next_frame`.
    pub enum Step {
        /// Trame de `n` octets utiles dans un tampon de `buffer` octets.
        Frame { used: usize, buffer: usize },
        /// Erreur `errno` brute.
        Errno(i32),
        /// Délai dépassé tel que produit par `v4l`.
        TimedOut,
    }

    /// Flux qui rejoue un script puis renvoie `EINVAL`.
    pub struct ScriptedStream {
        steps: VecDeque<Step>,
        current: Vec<u8>,
        sequence: u32,
        /// Délais successivement demandés, pour vérification.
        pub timeouts: Vec<Duration>,
    }

    impl ScriptedStream {
        pub fn new(steps: impl IntoIterator<Item = Step>) -> Self {
            Self {
                steps: steps.into_iter().collect(),
                current: Vec::new(),
                sequence: 0,
                timeouts: Vec::new(),
            }
        }
    }

    impl FrameStream for ScriptedStream {
        fn set_timeout(&mut self, timeout: Duration) {
            self.timeouts.push(timeout);
        }

        fn next_frame(&mut self) -> io::Result<RawFrame<'_>> {
            match self.steps.pop_front() {
                Some(Step::Frame { used, buffer }) => {
                    // JPEG minimal valide dans la taille utile, zéros au-delà.
                    self.current = vec![0; buffer];
                    self.current[..2].copy_from_slice(&[0xFF, 0xD8]);
                    self.current[used - 2..used].copy_from_slice(&[0xFF, 0xD9]);
                    self.sequence += 1;
                    Ok(RawFrame {
                        buffer: &self.current,
                        bytes_used: used,
                        sequence: self.sequence,
                    })
                }
                Some(Step::Errno(code)) => Err(io::Error::from_raw_os_error(code)),
                Some(Step::TimedOut) => {
                    Err(io::Error::new(io::ErrorKind::TimedOut, "VIDIOC_DQBUF"))
                }
                None => Err(io::Error::from_raw_os_error(libc::EINVAL)),
            }
        }
    }

    /// Erreur d'ouverture scriptée.
    #[derive(Debug, thiserror::Error)]
    #[error("scripted open failure")]
    pub struct OpenFailure;

    /// Fabrique qui produit des flux scriptés et compte les ouvertures.
    pub struct ScriptedOpener {
        pub scripts: VecDeque<Option<Vec<Step>>>,
        pub opened: usize,
    }

    impl StreamOpener for ScriptedOpener {
        type Stream = ScriptedStream;
        type Error = OpenFailure;

        fn open(&mut self) -> Result<(Self::Stream, OpenTimings), Self::Error> {
            match self.scripts.pop_front().flatten() {
                Some(steps) => {
                    self.opened += 1;
                    Ok((ScriptedStream::new(steps), OpenTimings::default()))
                }
                None => Err(OpenFailure),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_is_truncated_to_bytes_used() {
        let buffer = [1u8, 2, 3, 4, 5];
        let frame = RawFrame {
            buffer: &buffer,
            bytes_used: 3,
            sequence: 0,
        };
        assert_eq!(frame.payload(), &[1, 2, 3]);
    }

    #[test]
    fn payload_never_exceeds_buffer() {
        let buffer = [1u8, 2];
        let frame = RawFrame {
            buffer: &buffer,
            bytes_used: 10,
            sequence: 0,
        };
        assert_eq!(frame.payload(), &[1, 2]);
    }
}
