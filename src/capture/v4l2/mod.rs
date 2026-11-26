//! Adaptateur V4L2 (feature Cargo « v4l2 »), encapsulant la crate `v4l` 0.14.
//!
//! Seul module du projet qui touche au matériel. Enseignements du spike 0001
//! appliqués : troncature à `bytesused`, délai de `poll` borné, `Timeout` classé
//! `Reopen` (`EINVAL` après `TimedOut`), `Drop` de `v4l` sous `catch_unwind`,
//! classification des `errno`. Les paniques de `v4l` hors `Drop` (ouverture,
//! configuration, allocation, lecture) sont aussi interceptées (`guard`).

mod device;
mod errno;
mod guard;
mod release;
mod source;

pub use device::{Configured, Opened, V4l2Device};
pub use source::V4l2Source;

use crate::domain::{CaptureError, CaptureSettings};

/// Ouvre, configure et démarre une webcam V4L2 selon `settings`.
pub fn open_source(settings: &CaptureSettings) -> Result<V4l2Source, CaptureError> {
    V4l2Device::open(&settings.device)?
        .configure(settings)?
        .start_streaming(settings.read_timeout)
}
