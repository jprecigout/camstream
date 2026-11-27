//! Camstream : capture d'une webcam USB (V4L2, MJPEG natif) et diffusion vers les
//! navigateurs en `multipart/x-mixed-replace`, conçu pour un Raspberry Pi sous Docker.
//!
//! Architecture hexagonale (ADR 0001) :
//! - cœur : [`domain`] (types et ports) et [`stream`] (diffusion, supervision) ;
//! - adaptateurs : [`capture`] (V4L2, mire synthétique) et [`server`] (HTTP) ;
//! - [`config`] (CLI et environnement) et [`app`] (racine de composition) ;
//! - [`banner`] : bannière de démarrage (texte seul, affiché par le binaire).
//!
//! La bibliothèque est exposée pour les tests d'intégration (`tests/`) ; le binaire
//! `camstream` (`src/main.rs`) n'en est qu'un mince point d'entrée.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

// La robustesse de la capture repose sur le déroulement des paniques : `Released<T>`
// et les appels protégés de l'adaptateur V4L2 (`catch_unwind`), et le `PanicGuard` du
// superviseur (publie `Failed`). Avec `panic = "abort"`, ils seraient inopérants sans
// que rien ne le signale : on refuse donc de compiler.
#[cfg(panic = "abort")]
compile_error!(
    "camstream requires panic = \"unwind\" (catch_unwind in the V4L2 adapter and the \
     capture PanicGuard rely on it); remove `panic = \"abort\"` from the build profile"
);

pub mod app;
pub mod banner;
pub mod capture;
pub mod config;
pub mod domain;
pub mod server;
pub mod stream;
