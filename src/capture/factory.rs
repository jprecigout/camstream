//! Fabrique concrète des sources vidéo selon la configuration.

use crate::domain::{CaptureError, CaptureSettings, FrameSource, FrameSourceFactory, SourceKind};

use super::synthetic::SyntheticSource;

/// Fabrique concrète choisissant l'adaptateur selon `CaptureSettings::kind`.
///
/// Implémente le pattern **Factory** : seul point du code qui connaît les types
/// concrets de source. Appelée par le superviseur à la sonde de démarrage, à chaque
/// premier client et à chaque reconnexion.
#[derive(Debug, Clone)]
pub struct SourceFactory {
    settings: CaptureSettings,
}

impl SourceFactory {
    /// Crée la fabrique pour les paramètres donnés.
    pub fn new(settings: CaptureSettings) -> Self {
        Self { settings }
    }

    /// Paramètres utilisés à chaque création.
    pub fn settings(&self) -> &CaptureSettings {
        &self.settings
    }
}

impl FrameSourceFactory for SourceFactory {
    /// Ouvre, configure et démarre la source demandée.
    ///
    /// Sans la feature `v4l2`, demander une webcam donne
    /// [`CaptureError::SourceNotAvailable`] (fatale, visible dès la sonde).
    fn create(&self) -> Result<Box<dyn FrameSource>, CaptureError> {
        match self.settings.kind {
            SourceKind::Synthetic => Ok(Box::new(SyntheticSource::new(self.settings.fps))),
            #[cfg(feature = "v4l2")]
            SourceKind::V4l2 => Ok(Box::new(super::v4l2::open_source(&self.settings)?)),
            #[cfg(not(feature = "v4l2"))]
            SourceKind::V4l2 => Err(CaptureError::SourceNotAvailable(SourceKind::V4l2)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::domain::{DevicePath, FormatPreference, Fps, Recovery, Resolution};

    fn settings(kind: SourceKind) -> CaptureSettings {
        CaptureSettings {
            kind,
            device: DevicePath::new("/dev/camstream-test-missing"),
            resolution: Resolution::new(640, 480).unwrap(),
            fps: Fps::new(30).unwrap(),
            format: FormatPreference::Auto,
            read_timeout: Duration::from_secs(2),
        }
    }

    #[test]
    fn creates_a_synthetic_source() {
        let factory = SourceFactory::new(settings(SourceKind::Synthetic));
        let mut source = factory.create().unwrap();
        assert_eq!(source.description().fps.get(), 30);
        assert!(source.next_frame().is_ok());
    }

    #[test]
    fn missing_webcam_is_recoverable_or_unavailable() {
        let factory = SourceFactory::new(settings(SourceKind::V4l2));
        let error = factory.create().err().expect("no such device");
        if cfg!(feature = "v4l2") {
            assert!(matches!(error, CaptureError::DeviceNotFound(_)));
            assert_eq!(error.recovery(), Recovery::Reopen);
        } else {
            assert!(matches!(error, CaptureError::SourceNotAvailable(_)));
            assert_eq!(error.recovery(), Recovery::Fatal);
        }
    }
}
