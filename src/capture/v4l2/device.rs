//! Périphérique V4L2 en Typestate : ouvert → configuré → en streaming.
//!
//! La configuration énumère les capacités réelles du périphérique (formats, tailles,
//! cadences) via [`DeviceCapabilities`], choisit format, résolution et cadence avec
//! `capture::negotiation` puis prépare la stratégie de conversion en JPEG (ADR 0008).

use std::time::Duration;

use v4l::buffer::Type as BufferType;
use v4l::capability::Flags as CapabilityFlags;
use v4l::frameinterval::FrameIntervalEnum;
use v4l::framesize::FrameSizeEnum;
use v4l::prelude::{Device, MmapStream};
use v4l::video::capture::Parameters;
use v4l::video::Capture;
use v4l::{Format, FourCC, Fraction};

use super::errno::{map_io_error, map_open_error};
use super::guard::guarded;
use super::release::Released;
use super::source::V4l2Source;
use crate::capture::convert::{encoder_for, FrameEncoder};
use crate::capture::negotiation::{
    negotiate, CapabilityQuery, FourCc, FrameInterval, IntervalRange, Negotiated, SizeRange,
};
use crate::domain::{
    CaptureError, CaptureSettings, DevicePath, Fps, FpsOrigin, PixelFormat, Resolution,
    SourceDescription,
};

/// Nombre de tampons mmap demandés au pilote (valeur du spike 0001).
const BUFFER_COUNT: u32 = 4;

/// Délai maximal accepté par `MmapStream::set_timeout`, qui convertit en `i32` de
/// millisecondes avec `unwrap()` : on borne pour ne jamais paniquer.
const MAX_POLL_TIMEOUT: Duration = Duration::from_millis(i32::MAX as u64);

/// État « ouvert » : nœud ouvert et identifié comme périphérique de capture.
#[derive(Debug)]
pub struct Opened;

/// État « configuré » : format, résolution et cadence négociés avec le pilote, et
/// stratégie de conversion en JPEG prête.
pub struct Configured {
    negotiated: NegotiatedFormat,
    encoder: Box<dyn FrameEncoder>,
}

/// Paramètres retenus par le pilote (il peut ajuster résolution et cadence).
#[derive(Debug, Clone, Copy)]
struct NegotiatedFormat {
    pixel_format: PixelFormat,
    resolution: Resolution,
    fps: Fps,
    fps_origin: FpsOrigin,
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
    /// [`CaptureError::NotACaptureDevice`], fatale. Une panique de `v4l` (nom de carte
    /// non UTF-8, chemin contenant un octet nul) donne [`CaptureError::DriverFault`].
    pub fn open(path: &DevicePath) -> Result<Self, CaptureError> {
        let device = guarded("open", || {
            Device::with_path(path.as_path()).map_err(|e| map_open_error(path, e))
        })?;
        // Enveloppé avant `query_caps` : si celui-ci panique, le périphérique n'est pas
        // détruit pendant le déroulement (pas de double panique), mais après, sous
        // `catch_unwind`.
        let device = Released::new(device, "v4l device");
        let opened = device.get().ok_or(CaptureError::Disconnected)?;
        let caps = guarded("query_caps", || {
            opened.query_caps().map_err(|e| map_io_error(path, e))
        })?;
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

    /// Négocie format, résolution et cadence parmi les capacités énumérées
    /// (`capture::negotiation`), les applique (`S_FMT`, `S_PARM`) et prépare la
    /// conversion en JPEG.
    ///
    /// Erreurs fatales : [`CaptureError::UnsupportedFormat`] (aucun format ne
    /// correspond à `CAMSTREAM_FORMAT`, message listant l'offre de la caméra, ou
    /// pilote qui impose un autre format) et [`CaptureError::UnsupportedResolution`].
    /// Un pilote sans réglage de cadence (`S_PARM` en `ENOTTY`) n'est pas une erreur.
    pub fn configure(
        self,
        settings: &CaptureSettings,
    ) -> Result<V4l2Device<Configured>, CaptureError> {
        let device = self.device.get().ok_or(CaptureError::Disconnected)?;
        let capabilities = DeviceCapabilities {
            device,
            path: &self.path,
        };
        let negotiated = negotiate(&capabilities, settings)?;
        let Negotiated {
            fourcc,
            pixel_format,
            resolution: chosen_resolution,
            fps: chosen_fps,
        } = negotiated;
        let wanted = Format::new(
            chosen_resolution.width(),
            chosen_resolution.height(),
            FourCC::new(&fourcc.repr()),
        );
        // `Format::from` panique (`expect`) sur un champ inconnu de la version de `v4l`.
        let format = guarded("set_format", || {
            device
                .set_format(&wanted)
                .map_err(|e| map_io_error(&self.path, e))
        })?;
        if format.fourcc.repr != fourcc.repr() {
            let applied = FourCc::new(format.fourcc.repr);
            tracing::error!(device = %self.path, requested = %fourcc, applied = %applied,
                "the driver refused the negotiated pixel format");
            return Err(CaptureError::UnsupportedFormat {
                preference: settings.format,
                offered: format!("{applied} (imposed by the driver instead of {fourcc})"),
            });
        }
        let resolution = Resolution::new(format.width, format.height).unwrap_or(chosen_resolution);
        let (fps, fps_origin) = self.apply_frame_rate(device, chosen_fps)?;
        if resolution != chosen_resolution || fps != chosen_fps {
            tracing::info!(selected = %format_args!("{chosen_resolution} @ {chosen_fps} fps"),
                applied = %format_args!("{resolution} @ {fps} fps"),
                "the driver adjusted the capture parameters");
        }
        let stride = (format.stride > 0).then_some(format.stride);
        let encoder = encoder_for(pixel_format, resolution, stride, settings.encoding)?;
        Ok(V4l2Device {
            device: self.device,
            path: self.path,
            name: self.name,
            state: Configured {
                negotiated: NegotiatedFormat {
                    pixel_format,
                    resolution,
                    fps,
                    fps_origin,
                },
                encoder,
            },
        })
    }

    /// Demande la cadence `fps` (`S_PARM`) et renvoie celle retenue par le pilote, avec
    /// son origine : [`FpsOrigin::Requested`] si le pilote ne l'a pas confirmée.
    ///
    /// Un pilote qui ne gère pas le réglage de cadence (`ENOTTY`, fréquent sur les
    /// capteurs bruts) ou qui refuse la valeur demandée (`EINVAL`) garde sa cadence
    /// propre : on conserve la cadence choisie comme valeur nominale, avec un
    /// avertissement. Traiter ces refus déterministes comme une erreur `Reopen`
    /// rouvrirait la webcam indéfiniment sans jamais diffuser.
    fn apply_frame_rate(
        &self,
        device: &Device,
        fps: Fps,
    ) -> Result<(Fps, FpsOrigin), CaptureError> {
        let result = guarded("set_params", || {
            Ok(device.set_params(&Parameters::with_fps(fps.get())))
        })?;
        match result {
            Ok(params) => Ok(match fraction_to_fps(params.interval) {
                Some(applied) => (applied, FpsOrigin::Confirmed),
                // Intervalle dégénéré renvoyé par le pilote : rien de confirmé.
                None => (fps, FpsOrigin::Requested),
            }),
            Err(error) if matches!(error.raw_os_error(), Some(libc::ENOTTY | libc::EINVAL)) => {
                tracing::warn!(device = %self.path, requested_fps = %fps, %error,
                    "the driver does not support frame rate selection; using its own rate");
                Ok((fps, FpsOrigin::Requested))
            }
            Err(error) => Err(map_io_error(&self.path, error)),
        }
    }
}

impl V4l2Device<Configured> {
    /// Alloue les tampons mmap et prépare le flux ; consomme le périphérique.
    ///
    /// `STREAMON` est envoyé par `v4l` à la première lecture : la latence de
    /// démarrage de la webcam apparaît donc dans le premier `next_frame`.
    pub fn start_streaming(self, read_timeout: Duration) -> Result<V4l2Source, CaptureError> {
        let device = self.device.get().ok_or(CaptureError::Disconnected)?;
        // Un échec en cours d'allocation détruit l'`Arena` partielle dans `v4l`, dont le
        // `Drop` peut paniquer (`munmap`, `REQBUFS 0`) : appel protégé.
        let mut stream = guarded("mmap_buffers", || {
            MmapStream::with_buffers(device, BufferType::VideoCapture, BUFFER_COUNT)
                .map_err(|e| map_io_error(&self.path, e))
        })?;
        stream.set_timeout(read_timeout.min(MAX_POLL_TIMEOUT));
        let Configured {
            negotiated,
            encoder,
        } = self.state;
        let description = SourceDescription {
            name: self.name,
            pixel_format: negotiated.pixel_format,
            resolution: negotiated.resolution,
            fps: negotiated.fps,
            fps_origin: negotiated.fps_origin,
            encoding: encoder.encoding(),
        };
        Ok(V4l2Source::new(
            Released::new(stream, "v4l mmap stream"),
            self.device,
            self.path,
            description,
            encoder,
        ))
    }
}

/// Port [`CapabilityQuery`] au-dessus des ioctl d'énumération de `v4l`
/// (`VIDIOC_ENUM_FMT`, `VIDIOC_ENUM_FRAMESIZES`, `VIDIOC_ENUM_FRAMEINTERVALS`).
///
/// Implémente le pattern **Adapter** : traduit les types `v4l` vers ceux de la
/// négociation. Chaque appel est protégé (`guarded`) : `v4l` panique par exemple sur
/// une description de format non UTF-8.
pub struct DeviceCapabilities<'a> {
    device: &'a Device,
    path: &'a DevicePath,
}

impl CapabilityQuery for DeviceCapabilities<'_> {
    fn formats(&self) -> Result<Vec<FourCc>, CaptureError> {
        let formats = guarded("enum_formats", || {
            self.device
                .enum_formats()
                .map_err(|e| map_io_error(self.path, e))
        })?;
        Ok(formats
            .into_iter()
            .map(|description| FourCc::new(description.fourcc.repr))
            .collect())
    }

    fn sizes(&self, fourcc: FourCc) -> Result<Vec<SizeRange>, CaptureError> {
        let sizes = guarded("enum_framesizes", || {
            self.device
                .enum_framesizes(FourCC::new(&fourcc.repr()))
                .map_err(|e| map_io_error(self.path, e))
        })?;
        Ok(sizes
            .into_iter()
            .filter_map(|size| size_range(size.size))
            .collect())
    }

    fn intervals(
        &self,
        fourcc: FourCc,
        size: Resolution,
    ) -> Result<Vec<IntervalRange>, CaptureError> {
        let intervals = guarded("enum_frameintervals", || {
            self.device
                .enum_frameintervals(FourCC::new(&fourcc.repr()), size.width(), size.height())
                .map_err(|e| map_io_error(self.path, e))
        })?;
        Ok(intervals
            .into_iter()
            .map(|interval| interval_range(interval.interval))
            .collect())
    }
}

/// Plage de tailles `v4l` → [`SizeRange`] ; `None` pour une taille nulle (ignorée).
fn size_range(size: FrameSizeEnum) -> Option<SizeRange> {
    match size {
        FrameSizeEnum::Discrete(discrete) => {
            Resolution::new(discrete.width, discrete.height).map(SizeRange::Discrete)
        }
        FrameSizeEnum::Stepwise(stepwise) => Some(SizeRange::Stepwise {
            min: Resolution::new(stepwise.min_width.max(1), stepwise.min_height.max(1))?,
            max: Resolution::new(stepwise.max_width.max(1), stepwise.max_height.max(1))?,
            step_width: stepwise.step_width,
            step_height: stepwise.step_height,
        }),
    }
}

/// Plage d'intervalles `v4l` → [`IntervalRange`] (les intervalles dégénérés sont
/// écartés plus tard par `choose_fps`).
fn interval_range(interval: FrameIntervalEnum) -> IntervalRange {
    let convert = |fraction: Fraction| FrameInterval::new(fraction.numerator, fraction.denominator);
    match interval {
        FrameIntervalEnum::Discrete(fraction) => IntervalRange::Discrete(convert(fraction)),
        FrameIntervalEnum::Stepwise(stepwise) => IntervalRange::Stepwise {
            min: convert(stepwise.min),
            max: convert(stepwise.max),
        },
    }
}

/// Cadence entière arrondie correspondant à un intervalle `v4l` (`1/15` → 15).
fn fraction_to_fps(interval: Fraction) -> Option<Fps> {
    FrameInterval::new(interval.numerator, interval.denominator).to_fps()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v4l_ranges_are_translated() {
        use v4l::framesize::{Discrete, Stepwise};
        let res = |w, h| Resolution::new(w, h).unwrap();
        assert_eq!(
            size_range(FrameSizeEnum::Discrete(Discrete {
                width: 324,
                height: 240
            })),
            Some(SizeRange::Discrete(res(324, 240)))
        );
        assert_eq!(
            size_range(FrameSizeEnum::Discrete(Discrete {
                width: 0,
                height: 240
            })),
            None
        );
        assert_eq!(
            size_range(FrameSizeEnum::Stepwise(Stepwise {
                min_width: 0,
                max_width: 640,
                step_width: 2,
                min_height: 2,
                max_height: 480,
                step_height: 2,
            })),
            Some(SizeRange::Stepwise {
                min: res(1, 2),
                max: res(640, 480),
                step_width: 2,
                step_height: 2
            })
        );
        assert_eq!(
            interval_range(FrameIntervalEnum::Discrete(Fraction::new(1, 15))),
            IntervalRange::Discrete(FrameInterval::new(1, 15))
        );
        assert_eq!(
            fraction_to_fps(Fraction::new(1001, 30000)).map(Fps::get),
            Some(30)
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
