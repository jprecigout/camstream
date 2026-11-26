//! Déclaration de la ligne de commande et des variables d'environnement (clap).
//!
//! Chaque option a sa variable `CAMSTREAM_*` ; la ligne de commande a priorité sur
//! l'environnement. Les valeurs par défaut sont documentées dans
//! `docs/deploiement.md` § 5 et doivent rester synchronisées avec ce fichier.

use std::net::IpAddr;
use std::num::NonZeroUsize;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::domain::{FormatPreference, Resolution, SourceKind};

/// Ligne de commande de Camstream (les textes d'aide, techniques, sont en anglais).
#[derive(Debug, Parser)]
#[command(
    name = "camstream",
    version,
    about = "Stream a USB webcam to web browsers (MJPEG over HTTP)"
)]
pub struct Cli {
    /// Sous-commande ; sans sous-commande, l'application démarre le serveur.
    #[command(subcommand)]
    pub command: Option<CliCommand>,
    /// Adresse et port d'écoute (partagés avec `healthcheck`).
    #[command(flatten)]
    pub listen: ListenArgs,
    /// Options du serveur et de la capture.
    #[command(flatten)]
    pub serve: ServeArgs,
}

/// Sous-commandes.
#[derive(Debug, Subcommand)]
pub enum CliCommand {
    /// Interroge `/health` de l'instance locale ; code de sortie 0 si elle est saine,
    /// 1 sinon (sonde `HEALTHCHECK` de Docker).
    #[command(about = "Query /health on the local instance (exit code 0 if healthy, 1 otherwise)")]
    Healthcheck {
        /// Délai maximal de chaque étape de la requête, en millisecondes.
        #[arg(long, env = "CAMSTREAM_HEALTHCHECK_TIMEOUT_MS", default_value_t = 2000,
              value_parser = clap::value_parser!(u64).range(1..),
              help = "Request timeout in milliseconds")]
        timeout_ms: u64,
    },
}

/// Adresse d'écoute, utilisée par le serveur et par `healthcheck`.
#[derive(Debug, Args)]
pub struct ListenArgs {
    /// Adresse d'écoute HTTP (`127.0.0.1` par défaut ; l'image Docker fixe `0.0.0.0`).
    #[arg(
        long,
        env = "CAMSTREAM_BIND",
        default_value = "127.0.0.1",
        global = true,
        help = "HTTP listen address (the Docker image sets 0.0.0.0)"
    )]
    pub bind: IpAddr,
    /// Port HTTP.
    #[arg(
        long,
        env = "CAMSTREAM_PORT",
        default_value_t = 8080,
        global = true,
        help = "HTTP listen port"
    )]
    pub port: u16,
}

/// Options du serveur et de la capture.
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Type de source : webcam V4L2 ou mire synthétique (sans matériel).
    #[arg(long, env = "CAMSTREAM_SOURCE", value_enum, default_value_t = SourceArg::V4l2,
          help = "Video source: a V4L2 webcam or a synthetic test pattern (no hardware)")]
    pub source: SourceArg,
    /// Périphérique de capture V4L2.
    #[arg(
        long,
        env = "CAMSTREAM_DEVICE",
        default_value = "/dev/video0",
        help = "V4L2 capture device"
    )]
    pub device: PathBuf,
    /// Résolution demandée ; le pilote peut en retenir une proche.
    #[arg(
        long,
        env = "CAMSTREAM_RESOLUTION",
        default_value = "640x480",
        help = "Requested resolution (WIDTHxHEIGHT); the driver may pick a close one"
    )]
    pub resolution: Resolution,
    /// Cadence demandée (1 à 120 i/s).
    #[arg(long, env = "CAMSTREAM_FPS", default_value_t = 15,
          value_parser = clap::value_parser!(u32).range(1..=120),
          help = "Requested frame rate (frames per second)")]
    pub fps: u32,
    /// Format de pixels ; MJPEG obligatoire en v1 (`auto` = `mjpeg`, `yuyv` refusé).
    #[arg(long, env = "CAMSTREAM_FORMAT", value_enum, default_value_t = FormatArg::Auto,
          help = "Pixel format: MJPEG is required in v1 (auto = mjpeg, yuyv is refused)")]
    pub format: FormatArg,
    /// Attente maximale d'une trame avant de considérer la webcam figée (rouverte).
    #[arg(long, env = "CAMSTREAM_READ_TIMEOUT_MS", default_value_t = 2000,
          value_parser = clap::value_parser!(u64).range(1..),
          help = "Maximum wait for a frame (ms) before the camera is reopened")]
    pub read_timeout_ms: u64,
    /// Délai de grâce avant d'éteindre la webcam sans spectateur (0 : immédiat).
    #[arg(
        long,
        env = "CAMSTREAM_IDLE_TIMEOUT_MS",
        default_value_t = 10_000,
        help = "Grace period (ms) before turning the camera off without viewers (0: immediate)"
    )]
    pub idle_timeout_ms: u64,
    /// Nombre maximal de clients de flux simultanés (au-delà : HTTP 503).
    #[arg(
        long,
        env = "CAMSTREAM_MAX_CLIENTS",
        default_value = "10",
        help = "Maximum number of simultaneous stream clients (beyond: HTTP 503)"
    )]
    pub max_clients: NonZeroUsize,
    /// Trames conservées dans le tampon de diffusion (puissance de deux supérieure).
    #[arg(
        long,
        env = "CAMSTREAM_HUB_CAPACITY",
        default_value = "4",
        help = "Frames kept in the broadcast buffer (rounded up to a power of two)"
    )]
    pub hub_capacity: NonZeroUsize,
    /// Première attente avant reconnexion de la webcam.
    #[arg(long, env = "CAMSTREAM_RECONNECT_INITIAL_MS", default_value_t = 1000,
          value_parser = clap::value_parser!(u64).range(1..),
          help = "First wait (ms) before reconnecting the camera")]
    pub reconnect_initial_ms: u64,
    /// Attente maximale entre deux tentatives de reconnexion.
    #[arg(long, env = "CAMSTREAM_RECONNECT_MAX_MS", default_value_t = 30_000,
          value_parser = clap::value_parser!(u64).range(1..),
          help = "Maximum wait (ms) between two reconnection attempts")]
    pub reconnect_max_ms: u64,
    /// Durée maximale de l'arrêt gracieux.
    #[arg(long, env = "CAMSTREAM_SHUTDOWN_TIMEOUT_MS", default_value_t = 5000,
          value_parser = clap::value_parser!(u64).range(1..),
          help = "Maximum duration (ms) of the graceful shutdown")]
    pub shutdown_timeout_ms: u64,
}

/// Valeur CLI du type de source (miroir de [`SourceKind`] : `domain` ne dépend pas
/// de clap).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SourceArg {
    /// Webcam V4L2.
    #[value(help = "V4L2 webcam")]
    V4l2,
    /// Mire de test synthétique.
    #[value(help = "Synthetic test pattern, no hardware needed")]
    Synthetic,
}

impl From<SourceArg> for SourceKind {
    fn from(value: SourceArg) -> Self {
        match value {
            SourceArg::V4l2 => Self::V4l2,
            SourceArg::Synthetic => Self::Synthetic,
        }
    }
}

/// Valeur CLI de la préférence de format (miroir de [`FormatPreference`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FormatArg {
    /// MJPEG si disponible (seul format pris en charge en v1).
    #[value(help = "MJPEG if available (the only supported format in v1)")]
    Auto,
    /// MJPEG exigé.
    #[value(help = "Require MJPEG")]
    Mjpeg,
    /// YUYV forcé (phase 2, refusé en v1).
    #[value(help = "Force YUYV (phase 2, refused in v1)")]
    Yuyv,
}

impl From<FormatArg> for FormatPreference {
    fn from(value: FormatArg) -> Self {
        match value {
            FormatArg::Auto => Self::Auto,
            FormatArg::Mjpeg => Self::Mjpeg,
            FormatArg::Yuyv => Self::Yuyv,
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }
}
