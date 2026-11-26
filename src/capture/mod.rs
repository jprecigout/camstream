//! Adaptateurs de source : webcam V4L2 (feature `v4l2`), mire synthétique et fabrique.
//!
//! Ne dépend que de `domain` (et de la crate `v4l` pour l'adaptateur V4L2) : il ne
//! connaît ni le serveur HTTP, ni la configuration (ADR 0001).

pub mod factory;
pub mod jpeg;
pub mod synthetic;
#[cfg(feature = "v4l2")]
pub mod v4l2;

pub use factory::SourceFactory;
pub use synthetic::SyntheticSource;
