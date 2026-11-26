//! Adaptateur `v4l` 0.14 : énumération des formats, négociation MJPEG et flux mmap.
//!
//! Implémente le pattern **Adapter** : traduit l'API de la crate `v4l` vers les
//! traits `FrameStream` / `StreamOpener` du spike. C'est le seul module qui touche
//! au matériel ; il n'est donc pas couvert par les tests unitaires.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use v4l::buffer::Type as BufferType;
use v4l::frameinterval::FrameIntervalEnum;
use v4l::framesize::FrameSizeEnum;
use v4l::io::traits::CaptureStream;
use v4l::prelude::{Device, MmapStream};
use v4l::video::capture::Parameters;
use v4l::video::Capture;
use v4l::{Format, FourCC, Fraction};

use crate::error::{ErrnoKind, ProbeError};
use crate::stream::{FrameStream, OpenTimings, RawFrame, StreamOpener};

/// FourCC du MJPEG, seul format accepté en v1.
const MJPEG: &[u8; 4] = b"MJPG";

/// Délai maximal accepté par `MmapStream::set_timeout` (`i32::MAX` millisecondes).
const MAX_POLL_TIMEOUT: Duration = Duration::from_millis(i32::MAX as u64);

/// Informations d'identification du périphérique (`VIDIOC_QUERYCAP`).
#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    /// Pilote (ex. `uvcvideo`).
    pub driver: String,
    /// Nom de la carte (nom de la webcam).
    pub card: String,
    /// Bus (ex. `usb-0000:01:00.0-1.3`).
    pub bus: String,
}

/// Format proposé par la webcam et ses résolutions.
#[derive(Debug, Clone, Serialize)]
pub struct FormatInfo {
    /// FourCC (ex. `MJPG`, `YUYV`).
    pub fourcc: String,
    /// Description fournie par le pilote.
    pub description: String,
    /// Résolutions et cadences proposées.
    pub sizes: Vec<SizeInfo>,
}

/// Résolution proposée et cadences associées.
#[derive(Debug, Clone, Serialize)]
pub struct SizeInfo {
    /// Résolution (`640x480`) ou plage (`160x120..1920x1080 step 8x8`).
    pub size: String,
    /// Cadences en images par seconde (`30`, `15`, ou plage).
    pub fps: Vec<String>,
}

/// Format effectivement négocié avec le pilote.
#[derive(Debug, Clone, Serialize)]
pub struct Negotiated {
    /// FourCC retenu par le pilote.
    pub fourcc: String,
    /// Largeur retenue.
    pub width: u32,
    /// Hauteur retenue.
    pub height: u32,
    /// Taille maximale d'une image annoncée par le pilote (`sizeimage`).
    pub size_image: u32,
    /// Intervalle entre images retenu (`1/15`).
    pub interval: String,
    /// Cadence correspondante.
    pub fps: Option<f64>,
}

/// Paramètres de capture demandés.
#[derive(Debug, Clone)]
pub struct CaptureRequest {
    /// Nœud V4L2.
    pub path: PathBuf,
    /// Largeur demandée.
    pub width: u32,
    /// Hauteur demandée.
    pub height: u32,
    /// Cadence demandée.
    pub fps: u32,
    /// Nombre de tampons mmap.
    pub buffers: u32,
}

/// Ouvre le nœud en traduisant l'erreur système.
fn open_device(path: &Path) -> Result<Device, ProbeError> {
    Device::with_path(path).map_err(|source| ProbeError::Open {
        path: path.to_path_buf(),
        kind: ErrnoKind::classify(&source),
        source,
    })
}

/// Interroge l'identité du périphérique.
pub fn query_info(path: &Path) -> Result<DeviceInfo, ProbeError> {
    let device = open_device(path)?;
    let caps = device
        .query_caps()
        .map_err(|e| ProbeError::ioctl("query_caps", e))?;
    Ok(DeviceInfo {
        driver: caps.driver,
        card: caps.card,
        bus: caps.bus,
    })
}

/// Énumère formats, résolutions et cadences proposés par la webcam.
///
/// Les erreurs d'énumération des tailles ou des cadences ne sont pas fatales :
/// certains pilotes ne les implémentent pas ; la liste est alors vide.
pub fn list_formats(path: &Path) -> Result<Vec<FormatInfo>, ProbeError> {
    let device = open_device(path)?;
    let descriptions = device
        .enum_formats()
        .map_err(|e| ProbeError::ioctl("enum_formats", e))?;
    let formats = descriptions
        .into_iter()
        .map(|description| {
            let sizes = device
                .enum_framesizes(description.fourcc)
                .unwrap_or_default()
                .into_iter()
                .map(|frame_size| match frame_size.size {
                    FrameSizeEnum::Discrete(size) => SizeInfo {
                        size: format!("{}x{}", size.width, size.height),
                        fps: device
                            .enum_frameintervals(description.fourcc, size.width, size.height)
                            .unwrap_or_default()
                            .into_iter()
                            .map(|interval| match interval.interval {
                                FrameIntervalEnum::Discrete(fraction) => format_fps(fraction),
                                FrameIntervalEnum::Stepwise(step) => {
                                    format!("{}..{}", format_fps(step.max), format_fps(step.min))
                                }
                            })
                            .collect(),
                    },
                    FrameSizeEnum::Stepwise(step) => SizeInfo {
                        size: format!(
                            "{}x{}..{}x{} step {}x{}",
                            step.min_width,
                            step.min_height,
                            step.max_width,
                            step.max_height,
                            step.step_width,
                            step.step_height
                        ),
                        fps: Vec::new(),
                    },
                })
                .collect();
            FormatInfo {
                fourcc: fourcc_text(description.fourcc),
                description: description.description,
                sizes,
            }
        })
        .collect();
    Ok(formats)
}

/// Texte d'un FourCC, sans échouer sur des octets non UTF-8.
fn fourcc_text(fourcc: FourCC) -> String {
    String::from_utf8_lossy(&fourcc.repr).into_owned()
}

/// Cadence correspondant à un intervalle (`1/15` → 15 i/s).
fn fraction_fps(fraction: Fraction) -> Option<f64> {
    (fraction.numerator != 0)
        .then(|| f64::from(fraction.denominator) / f64::from(fraction.numerator))
}

/// Cadence lisible (`15` ou `7.5`), ou l'intervalle brut si le numérateur est nul.
fn format_fps(fraction: Fraction) -> String {
    match fraction_fps(fraction) {
        Some(fps) => format!("{fps:.3}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned(),
        None => format!("{}/{}", fraction.numerator, fraction.denominator),
    }
}

/// Flux V4L2 en mmap.
///
/// **RAII** : `stream` est déclaré avant `device`, il est donc détruit en premier
/// (`STREAMOFF`, `munmap`, `REQBUFS 0`). Le descripteur, partagé par `Arc` entre les
/// deux, n'est fermé qu'à la libération de la dernière référence, soit après le flux.
pub struct V4l2Stream {
    stream: MmapStream<'static>,
    _device: Device,
}

impl FrameStream for V4l2Stream {
    fn set_timeout(&mut self, timeout: Duration) {
        // `v4l` 0.14 convertit le délai en `i32` de millisecondes avec `unwrap()` :
        // on borne pour ne jamais paniquer sur une valeur de `--timeout-ms` excessive.
        self.stream.set_timeout(timeout.min(MAX_POLL_TIMEOUT));
    }

    fn next_frame(&mut self) -> io::Result<RawFrame<'_>> {
        let (buffer, meta) = CaptureStream::next(&mut self.stream)?;
        Ok(RawFrame {
            buffer,
            bytes_used: meta.bytesused as usize,
            sequence: meta.sequence,
        })
    }
}

/// Fabrique de flux V4L2 MJPEG.
///
/// Conserve le format négocié lors de la dernière ouverture, pour le rapport.
pub struct V4l2Opener {
    request: CaptureRequest,
    /// Format retenu à la dernière ouverture réussie.
    pub negotiated: Option<Negotiated>,
}

impl V4l2Opener {
    /// Crée la fabrique pour les paramètres demandés.
    pub fn new(request: CaptureRequest) -> Self {
        Self {
            request,
            negotiated: None,
        }
    }
}

impl StreamOpener for V4l2Opener {
    type Stream = V4l2Stream;
    type Error = ProbeError;

    fn open(&mut self) -> Result<(Self::Stream, OpenTimings), Self::Error> {
        let request = &self.request;

        let started = Instant::now();
        let device = open_device(&request.path)?;
        let open = started.elapsed();

        let started = Instant::now();
        let wanted = Format::new(request.width, request.height, FourCC::new(MJPEG));
        let format = device
            .set_format(&wanted)
            .map_err(|e| ProbeError::ioctl("set_format", e))?;
        if format.fourcc != FourCC::new(MJPEG) {
            return Err(ProbeError::MjpegUnsupported {
                negotiated: fourcc_text(format.fourcc),
            });
        }
        let params = device
            .set_params(&Parameters::with_fps(request.fps))
            .map_err(|e| ProbeError::ioctl("set_params", e))?;
        let configure = started.elapsed();

        let started = Instant::now();
        let stream = MmapStream::with_buffers(&device, BufferType::VideoCapture, request.buffers)
            .map_err(|e| ProbeError::ioctl("mmap_buffers", e))?;
        let buffers = started.elapsed();

        self.negotiated = Some(Negotiated {
            fourcc: fourcc_text(format.fourcc),
            width: format.width,
            height: format.height,
            size_image: format.size,
            interval: format!(
                "{}/{}",
                params.interval.numerator, params.interval.denominator
            ),
            fps: fraction_fps(params.interval),
        });

        Ok((
            V4l2Stream {
                stream,
                _device: device,
            },
            OpenTimings {
                open,
                configure,
                buffers,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_frame_intervals_as_fps() {
        let fraction = |numerator, denominator| Fraction {
            numerator,
            denominator,
        };
        assert_eq!(format_fps(fraction(1, 15)), "15");
        assert_eq!(format_fps(fraction(2, 15)), "7.5");
        assert_eq!(format_fps(fraction(1, 30)), "30");
        assert_eq!(format_fps(fraction(0, 1)), "0/1");
    }

    #[test]
    fn fourcc_text_is_lossy_for_invalid_bytes() {
        assert_eq!(fourcc_text(FourCC::new(b"MJPG")), "MJPG");
        assert_eq!(
            fourcc_text(FourCC::new(&[0xFF, b'A', b'B', b'C'])),
            "\u{FFFD}ABC"
        );
    }

    #[test]
    fn opening_a_missing_device_is_classified_as_enoent() {
        let error = query_info(Path::new("/dev/camstream-spike-missing"))
            .expect_err("le nœud n'existe pas");
        assert!(matches!(
            error,
            ProbeError::Open {
                kind: ErrnoKind::Enoent,
                ..
            }
        ));
    }
}
