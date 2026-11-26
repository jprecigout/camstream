//! Cœur métier : types de trames, paramètres de capture et ports (traits) de source.
//!
//! Ce module ne dépend que de `bytes` et `thiserror` : ni V4L2, ni HTTP, ni CLI
//! (architecture hexagonale, ADR 0001).

pub mod device;
pub mod frame;
pub mod source;

pub use device::{CaptureSettings, DevicePath, FormatPreference, SourceKind};
pub use frame::{Fps, Frame, ParseResolutionError, PixelFormat, Resolution, SequenceNumber};
pub use source::{CaptureError, FrameSource, FrameSourceFactory, Recovery, SourceDescription};
