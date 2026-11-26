//! Camstream : capture d'une webcam USB (V4L2, MJPEG natif) et diffusion vers les
//! navigateurs en `multipart/x-mixed-replace`, conçu pour un Raspberry Pi sous Docker.
//!
//! Architecture hexagonale (ADR 0001) :
//! - cœur : [`domain`] (types et ports) et [`stream`] (diffusion, supervision) ;
//! - adaptateurs : [`capture`] (V4L2, mire synthétique) et [`server`] (HTTP) ;
//! - [`config`] (CLI et environnement) et [`app`] (racine de composition).
//!
//! La bibliothèque est exposée pour les tests d'intégration (`tests/`) ; le binaire
//! `camstream` (`src/main.rs`) n'en est qu'un mince point d'entrée.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod app;
pub mod capture;
pub mod config;
pub mod domain;
pub mod server;
pub mod stream;
