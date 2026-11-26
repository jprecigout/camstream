//! Source V4L2 en streaming : implémentation du port `FrameSource` sur un flux mmap.

use bytes::Bytes;
use v4l::buffer::Flags as BufferFlags;
use v4l::io::traits::CaptureStream;
use v4l::prelude::{Device, MmapStream};

use super::errno::map_io_error;
use super::guard::guarded;
use super::release::Released;
use crate::capture::jpeg::complete_jpeg_len;
use crate::domain::{
    CaptureError, DevicePath, Frame, FrameSource, SequenceNumber, SourceDescription,
};

/// Source V4L2 en streaming.
///
/// Implémente le pattern **Adapter** (flux mmap de `v4l` → port `FrameSource`) et
/// **RAII** : sa destruction arrête le flux (`STREAMOFF`), libère les tampons
/// (`munmap`) puis ferme le descripteur, sans propager une éventuelle panique des
/// `Drop` de `v4l` (enveloppe `Released`, module `release`).
pub struct V4l2Source {
    // Ordre des champs = ordre de destruction : le flux avant le périphérique.
    stream: Released<MmapStream<'static>>,
    _device: Released<Device>,
    path: DevicePath,
    description: SourceDescription,
    sequence: SequenceNumber,
}

impl V4l2Source {
    /// Assemble une source à partir d'un flux prêt (appelé par `start_streaming`).
    pub(super) fn new(
        stream: Released<MmapStream<'static>>,
        device: Released<Device>,
        path: DevicePath,
        description: SourceDescription,
    ) -> Self {
        Self {
            stream,
            _device: device,
            path,
            description,
            sequence: SequenceNumber::default(),
        }
    }
}

impl FrameSource for V4l2Source {
    /// Lit la trame suivante (bloquant, borné par le délai de lecture).
    ///
    /// Le tampon mmap est **tronqué à `bytesused`** (`v4l` renvoie le tampon entier)
    /// puis vérifié (`SOI`/`EOI`, bourrage retiré) et copié dans un `Bytes` : c'est la
    /// seule copie de l'image dans toute la chaîne, inévitable car le tampon est rendu
    /// au pilote à la lecture suivante.
    fn next_frame(&mut self) -> Result<Frame, CaptureError> {
        let stream = self.stream.get_mut().ok_or(CaptureError::Disconnected)?;
        let path = &self.path;
        // `v4l` indexe ses métadonnées par l'index de tampon renvoyé par le pilote :
        // une valeur inattendue paniquerait. La copie a lieu dans l'appel protégé, car
        // le tampon emprunté ne peut pas en sortir.
        let data = guarded("dequeue", || {
            let (buffer, meta) = CaptureStream::next(stream).map_err(|e| map_io_error(path, e))?;
            if meta.flags.contains(BufferFlags::ERROR) {
                return Err(CaptureError::CorruptedFrame);
            }
            let used = usize::try_from(meta.bytesused)
                .unwrap_or(usize::MAX)
                .min(buffer.len());
            let payload = &buffer[..used];
            let len = complete_jpeg_len(payload).ok_or(CaptureError::CorruptedFrame)?;
            Ok(Bytes::copy_from_slice(&payload[..len]))
        })?;
        let frame = Frame::new(data, self.description.resolution, self.sequence);
        self.sequence = self.sequence.next();
        Ok(frame)
    }

    fn description(&self) -> SourceDescription {
        self.description.clone()
    }
}
