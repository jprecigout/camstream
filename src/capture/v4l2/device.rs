//! Périphérique V4L2 en Typestate : ouvert → configuré → en streaming.

use std::time::Duration;

use v4l::buffer::Type as BufferType;
use v4l::capability::Flags as CapabilityFlags;
use v4l::prelude::{Device, MmapStream};
use v4l::video::capture::Parameters;
use v4l::video::Capture;
use v4l::{Format, FourCC, Fraction};

use super::errno::{map_io_error, map_open_error};
use super::release::Released;
use super::source::V4l2Source;
use crate::domain::{
    CaptureError, CaptureSettings, DevicePath, FormatPreference, Fps, PixelFormat, Resolution,
    SourceDescription,
};

/// FourCC du MJPEG, seul format accepté en v1.
const MJPEG: FourCC = FourCC { repr: *b"MJPG" };

/// Nombre de tampons mmap demandés au pilote (valeur du spike 0001).
const BUFFER_COUNT: u32 = 4;

/// Délai maximal accepté par `MmapStream::set_timeout`, qui convertit en `i32` de
/// millisecondes avec `unwrap()` : on borne pour ne jamais paniquer.
const MAX_POLL_TIMEOUT: Duration = Duration::from_millis(i32::MAX as u64);

/// État « ouvert » : nœud ouvert et identifié comme périphérique de capture.
#[derive(Debug)]
pub struct Opened;

/// État « configuré » : format, résolution et cadence négociés avec le pilote.
#[derive(Debug)]
pub struct Configured {
    negotiated: NegotiatedFormat,
}

/// Paramètres retenus par le pilote (il peut ajuster résolution et cadence).
#[derive(Debug, Clone, Copy)]
struct NegotiatedFormat {
    resolution: Resolution,
    fps: Fps,
}

/// Périphérique V4L2 dans l'état `S`.
///
/// Implémente les patterns **Adapter** (masque l'API `v4l`) et **Typestate** : un
/// périphérique ne peut démarrer le streaming qu'après avoir été configuré, ce que
/// le compilateur vérifie (changer le format après `STREAMON` échouerait en `EBUSY`).
pub struct V4l2Device<S> {
    device: Released<Device>,
    path: DevicePath,
    name: String,
    state: S,
}

impl V4l2Device<Opened> {
    /// Ouvre le périphérique et vérifie qu'il sait capturer en streaming.
    ///
    /// Traduit `ENOENT`/`EACCES`/`EPERM`/`ENODEV` en [`CaptureError`] typées. Un nœud
    /// sans capture vidéo (nœud de métadonnées d'une webcam UVC, par exemple) donne
    /// [`CaptureError::NotACaptureDevice`], fatale.
    pub fn open(path: &DevicePath) -> Result<Self, CaptureError> {
        let device = Device::with_path(path.as_path()).map_err(|e| map_open_error(path, e))?;
        let device = Released::new(device, "v4l device");
        let caps = device
            .get()
            .ok_or(CaptureError::Disconnected)?
            .query_caps()
            .map_err(|e| map_io_error(path, e))?;
        // `v4l` renvoie les capacités du nœud (`device_caps`), pas celles de la carte.
        if !caps
            .capabilities
            .contains(CapabilityFlags::VIDEO_CAPTURE | CapabilityFlags::STREAMING)
        {
            return Err(CaptureError::NotACaptureDevice(path.clone()));
        }
        tracing::debug!(device = %path, card = %caps.card, driver = %caps.driver, "video device opened");
        Ok(Self {
            device,
            path: path.clone(),
            name: caps.card,
            state: Opened,
        })
    }

    /// Négocie format, résolution et cadence (MJPEG obligatoire en v1 : `Auto` et
    /// `Mjpeg` exigent MJPEG, `Yuyv` est refusé jusqu'à la phase 2).
    pub fn configure(
        self,
        settings: &CaptureSettings,
    ) -> Result<V4l2Device<Configured>, CaptureError> {
        let preference = settings.format;
        if preference == FormatPreference::Yuyv {
            return Err(CaptureError::UnsupportedFormat { preference });
        }
        let device = self.device.get().ok_or(CaptureError::Disconnected)?;
        let wanted = Format::new(
            settings.resolution.width(),
            settings.resolution.height(),
            MJPEG,
        );
        let format = device
            .set_format(&wanted)
            .map_err(|e| map_io_error(&self.path, e))?;
        if format.fourcc != MJPEG {
            tracing::error!(device = %self.path, negotiated = %fourcc_text(format.fourcc),
                "the camera does not deliver MJPEG (required in v1)");
            return Err(CaptureError::UnsupportedFormat { preference });
        }
        let params = device
            .set_params(&Parameters::with_fps(settings.fps.get()))
            .map_err(|e| map_io_error(&self.path, e))?;

        let resolution =
            Resolution::new(format.width, format.height).unwrap_or(settings.resolution);
        let fps = fraction_to_fps(params.interval).unwrap_or(settings.fps);
        if resolution != settings.resolution || fps != settings.fps {
            tracing::info!(requested = %format_args!("{} @ {} fps", settings.resolution, settings.fps),
                negotiated = %format_args!("{resolution} @ {fps} fps"),
                "the driver adjusted the capture parameters");
        }
        Ok(V4l2Device {
            device: self.device,
            path: self.path,
            name: self.name,
            state: Configured {
                negotiated: NegotiatedFormat { resolution, fps },
            },
        })
    }
}

impl V4l2Device<Configured> {
    /// Alloue les tampons mmap et prépare le flux ; consomme le périphérique.
    ///
    /// `STREAMON` est envoyé par `v4l` à la première lecture : la latence de
    /// démarrage de la webcam apparaît donc dans le premier `next_frame`.
    pub fn start_streaming(self, read_timeout: Duration) -> Result<V4l2Source, CaptureError> {
        let device = self.device.get().ok_or(CaptureError::Disconnected)?;
        let mut stream = MmapStream::with_buffers(device, BufferType::VideoCapture, BUFFER_COUNT)
            .map_err(|e| map_io_error(&self.path, e))?;
        stream.set_timeout(read_timeout.min(MAX_POLL_TIMEOUT));
        let negotiated = self.state.negotiated;
        let description = SourceDescription {
            name: self.name,
            pixel_format: PixelFormat::Mjpeg,
            resolution: negotiated.resolution,
            fps: negotiated.fps,
        };
        Ok(V4l2Source::new(
            Released::new(stream, "v4l mmap stream"),
            self.device,
            self.path,
            description,
        ))
    }
}

/// Texte d'un FourCC, sans échouer sur des octets non UTF-8.
fn fourcc_text(fourcc: FourCC) -> String {
    String::from_utf8_lossy(&fourcc.repr).into_owned()
}

/// Cadence entière arrondie correspondant à un intervalle (`1/15` → 15) ; `None`
/// si l'intervalle est dégénéré ou donne moins de 0,5 image par seconde.
fn fraction_to_fps(interval: Fraction) -> Option<Fps> {
    if interval.numerator == 0 {
        return None;
    }
    // Arrondi au plus proche : 1001/30000 (29,97 i/s) donne 30.
    let fps = (u64::from(interval.denominator) + u64::from(interval.numerator) / 2)
        / u64::from(interval.numerator);
    Fps::new(u32::try_from(fps).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_intervals_become_rounded_fps() {
        let fps = |numerator, denominator| {
            fraction_to_fps(Fraction::new(numerator, denominator)).map(Fps::get)
        };
        assert_eq!(fps(1, 15), Some(15));
        assert_eq!(fps(1001, 30000), Some(30));
        assert_eq!(fps(2, 15), Some(8));
        assert_eq!(fps(0, 15), None);
        assert_eq!(fps(2, 1), Some(1));
        assert_eq!(fps(3, 1), None);
    }

    #[test]
    fn fourcc_text_is_lossy() {
        assert_eq!(fourcc_text(MJPEG), "MJPG");
        assert_eq!(
            fourcc_text(FourCC {
                repr: [0xFF, b'A', b'B', b'C']
            }),
            "\u{FFFD}ABC"
        );
    }

    #[test]
    fn opening_a_missing_node_is_device_not_found() {
        let path = DevicePath::new("/dev/camstream-test-missing");
        let error = V4l2Device::open(&path)
            .err()
            .expect("the node does not exist");
        assert!(matches!(error, CaptureError::DeviceNotFound(p) if p == path));
    }

    #[test]
    fn opening_a_non_video_node_is_rejected() {
        // `/dev/null` s'ouvre mais ne répond pas à VIDIOC_QUERYCAP (ENOTTY) : erreur
        // d'E/S, jamais une panique.
        let error = V4l2Device::open(&DevicePath::new("/dev/null"))
            .err()
            .expect("/dev/null is not a video device");
        assert!(matches!(
            error,
            CaptureError::Io(_) | CaptureError::NotACaptureDevice(_)
        ));
    }
}
