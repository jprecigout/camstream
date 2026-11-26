//! Traduction des erreurs système de V4L2 (`errno`) en [`CaptureError`] typées.
//!
//! Table validée par lecture du code de `v4l` 0.14 et par le spike 0001 ; à
//! confirmer sur le Raspberry Pi (docs/spikes/0001-v4l.md § 4).

use std::io;

use crate::domain::{CaptureError, DevicePath};

/// Traduit une erreur d'**ouverture** du nœud.
///
/// `EPERM` est le code renvoyé par le cgroup de périphériques de Docker ; `ENODEV` /
/// `ENXIO` à l'ouverture signifient « nœud présent mais webcam débranchée » : c'est
/// une déconnexion (reprise possible), pas une erreur fatale.
pub(crate) fn map_open_error(path: &DevicePath, error: io::Error) -> CaptureError {
    match error.raw_os_error() {
        Some(libc::ENOENT) => CaptureError::DeviceNotFound(path.clone()),
        Some(libc::EACCES | libc::EPERM) => CaptureError::PermissionDenied(path.clone()),
        Some(libc::ENODEV | libc::ENXIO) => CaptureError::Disconnected,
        _ => CaptureError::Io(error),
    }
}

/// Traduit une erreur d'ioctl ou de lecture d'un périphérique déjà ouvert.
///
/// `v4l` fabrique le dépassement de délai de `poll` sans `errno`
/// (`io::ErrorKind::TimedOut`) : il est testé en premier. `EINVAL` (attendu après
/// un `TimedOut`) et `EBUSY` restent des erreurs d'E/S génériques, donc `Reopen`.
pub(crate) fn map_io_error(path: &DevicePath, error: io::Error) -> CaptureError {
    if error.kind() == io::ErrorKind::TimedOut && error.raw_os_error().is_none() {
        return CaptureError::Timeout;
    }
    match error.raw_os_error() {
        Some(libc::ENODEV | libc::ENXIO) => CaptureError::Disconnected,
        Some(libc::EACCES | libc::EPERM) => CaptureError::PermissionDenied(path.clone()),
        Some(libc::ETIMEDOUT) => CaptureError::Timeout,
        _ => CaptureError::Io(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Recovery;

    fn path() -> DevicePath {
        DevicePath::new("/dev/video0")
    }

    fn os(code: i32) -> io::Error {
        io::Error::from_raw_os_error(code)
    }

    #[test]
    fn open_errors_are_classified() {
        let cases = [
            (
                libc::ENOENT,
                Recovery::Reopen,
                "video device not found: /dev/video0",
            ),
            (
                libc::EACCES,
                Recovery::Fatal,
                "permission denied on video device: /dev/video0",
            ),
            (
                libc::EPERM,
                Recovery::Fatal,
                "permission denied on video device: /dev/video0",
            ),
            (libc::ENODEV, Recovery::Reopen, "video device disconnected"),
            (libc::ENXIO, Recovery::Reopen, "video device disconnected"),
            (libc::EBUSY, Recovery::Reopen, "video I/O error"),
        ];
        for (code, recovery, message) in cases {
            let error = map_open_error(&path(), os(code));
            assert_eq!(error.recovery(), recovery, "errno {code}");
            assert_eq!(error.to_string(), message, "errno {code}");
        }
    }

    #[test]
    fn capture_errors_are_classified() {
        assert!(matches!(
            map_io_error(&path(), os(libc::ENODEV)),
            CaptureError::Disconnected
        ));
        assert!(matches!(
            map_io_error(&path(), os(libc::EINVAL)),
            CaptureError::Io(_)
        ));
        assert!(matches!(
            map_io_error(&path(), os(libc::EACCES)),
            CaptureError::PermissionDenied(_)
        ));
        assert!(matches!(
            map_io_error(&path(), os(libc::ETIMEDOUT)),
            CaptureError::Timeout
        ));
    }

    #[test]
    fn v4l_poll_timeout_becomes_timeout() {
        // Forme exacte produite par `v4l` 0.14 (`dequeue`) quand `poll` expire.
        let error = io::Error::new(io::ErrorKind::TimedOut, "VIDIOC_DQBUF");
        assert!(matches!(
            map_io_error(&path(), error),
            CaptureError::Timeout
        ));
    }
}
