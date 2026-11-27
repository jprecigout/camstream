//! Adaptateurs de source : webcam V4L2 (feature `v4l2`), mires synthétiques et fabrique,
//! ainsi que la négociation des capacités et la conversion des formats bruts en JPEG
//! (ADR 0008).
//!
//! Ne dépend que de `domain` (et des crates `v4l` pour l'adaptateur V4L2 et
//! `jpeg-encoder` pour la conversion) : il ne connaît ni le serveur HTTP, ni la
//! configuration (ADR 0001).

pub mod convert;
pub mod factory;
pub mod jpeg;
pub mod negotiation;
pub mod synthetic;
pub mod synthetic_raw;
#[cfg(feature = "v4l2")]
pub mod v4l2;

pub use factory::SourceFactory;
pub use synthetic::SyntheticSource;
pub use synthetic_raw::SyntheticRawSource;
