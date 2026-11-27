//! Configuration : arguments de ligne de commande et variables d'environnement.
//!
//! La configuration est lue **une seule fois** au démarrage puis découpée et injectée
//! dans chaque composant par `app` : aucun composant ne lit l'environnement
//! lui-même. Ne dépend que de `domain` (newtypes) et de `clap`.

pub mod cli;

use std::ffi::OsString;
use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroUsize};
use std::time::Duration;

use clap::Parser;

use crate::domain::{
    CaptureSettings, DevicePath, EncodingSettings, FormatPreference, Fps, JpegQuality,
};
use cli::{Cli, CliCommand, ServeArgs};

/// Capacité maximale acceptée pour le tampon de diffusion (mémoire du Pi).
pub const MAX_HUB_CAPACITY: usize = 64;

/// Nombre maximal accepté pour `--max-clients`.
///
/// Bien au-delà du débit d'un Pi (≈ 0,5 à 1 Mo/s par client) ; borne surtout la valeur
/// passée à `tokio::sync::Semaphore::new`, qui panique au-delà de `MAX_PERMITS`.
pub const MAX_CLIENTS_LIMIT: usize = 1000;

/// Erreurs de configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Argument ou variable mal formé, ou `--help`/`--version` (message de clap).
    #[error(transparent)]
    Parse(#[from] clap::Error),
    /// Combinaison de valeurs incohérente.
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

/// Ce que l'invocation demande de faire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Démarrer le serveur de diffusion.
    Serve(AppConfig),
    /// Interroger `/health` de l'instance locale (sonde `HEALTHCHECK` Docker).
    Healthcheck(HealthcheckConfig),
}

impl Command {
    /// Lit la CLI et l'environnement (`CAMSTREAM_*`), applique les défauts, valide.
    pub fn load() -> Result<Self, ConfigError> {
        Self::from_cli(Cli::try_parse()?)
    }

    /// Variante testable à partir d'arguments explicites.
    ///
    /// Attention : les variables `CAMSTREAM_*` du processus sont tout de même lues
    /// (feature `env` de clap) pour les options absentes de `args`.
    pub fn try_parse_from<I, T>(args: I) -> Result<Self, ConfigError>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        Self::from_cli(Cli::try_parse_from(args)?)
    }

    fn from_cli(cli: Cli) -> Result<Self, ConfigError> {
        let bind = SocketAddr::new(cli.listen.bind, cli.listen.port);
        match cli.command {
            Some(CliCommand::Healthcheck { timeout_ms }) => {
                Ok(Self::Healthcheck(HealthcheckConfig {
                    bind,
                    timeout: Duration::from_millis(timeout_ms),
                }))
            }
            None => AppConfig::from_args(bind, cli.serve).map(Self::Serve),
        }
    }
}

/// Paramètres de la sous-commande `healthcheck`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthcheckConfig {
    /// Adresse d'écoute de l'instance à interroger (`CAMSTREAM_BIND`/`CAMSTREAM_PORT`).
    pub bind: SocketAddr,
    /// Durée totale maximale de la sonde (connexion, envoi, lecture).
    pub timeout: Duration,
}

/// Configuration complète et validée de l'application.
///
/// Construite une seule fois au démarrage puis découpée et injectée dans chaque
/// composant : aucun composant ne lit l'environnement lui-même.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppConfig {
    /// Paramètres de la source vidéo.
    pub capture: CaptureSettings,
    /// Adresse d'écoute HTTP.
    pub bind: SocketAddr,
    /// Limite de clients simultanés.
    pub max_clients: NonZeroUsize,
    /// Capacité du tampon de diffusion.
    pub hub_capacity: NonZeroUsize,
    /// Première attente avant reconnexion de la webcam.
    pub reconnect_initial: Duration,
    /// Attente maximale entre deux tentatives de reconnexion.
    pub reconnect_max: Duration,
    /// Délai de grâce avant d'éteindre la webcam sans client (capture à la demande).
    pub idle_timeout: Duration,
    /// Délai maximal accordé à l'arrêt gracieux.
    pub shutdown_timeout: Duration,
    /// Trames invalides consécutives tolérées avant de rouvrir la webcam.
    pub max_corrupted_frames: NonZeroU32,
    /// Affiche la bannière de démarrage (désactivable par `--no-banner`).
    pub banner: bool,
}

impl AppConfig {
    /// Valide les arguments et construit la configuration.
    fn from_args(bind: SocketAddr, args: ServeArgs) -> Result<Self, ConfigError> {
        let format = FormatPreference::from(args.format);
        // Le parseur clap borne déjà la qualité à 1..=100 : `JpegQuality::new` ne peut
        // échouer.
        let quality = JpegQuality::new(args.jpeg_quality).ok_or_else(|| {
            ConfigError::Invalid("--jpeg-quality must be between 1 and 100".into())
        })?;
        // Le parseur clap borne déjà la cadence à 1..=120 : `Fps::new` ne peut échouer.
        let fps = Fps::new(args.fps)
            .ok_or_else(|| ConfigError::Invalid("--fps must be at least 1".into()))?;
        let read_timeout = Duration::from_millis(args.read_timeout_ms);
        if read_timeout <= fps.frame_interval() {
            return Err(ConfigError::Invalid(format!(
                "--read-timeout-ms ({} ms) must exceed the frame interval at {fps} fps ({} ms)",
                args.read_timeout_ms,
                fps.frame_interval().as_millis()
            )));
        }
        // À l'arrêt, le thread de capture n'observe le jeton qu'entre deux lectures :
        // une lecture bloquée jusqu'au délai de lecture doit tenir dans le délai d'arrêt,
        // sinon le join expire à chaque arrêt d'une webcam figée.
        if args.read_timeout_ms >= args.shutdown_timeout_ms {
            return Err(ConfigError::Invalid(format!(
                "--read-timeout-ms ({} ms) must be shorter than --shutdown-timeout-ms ({} ms): \
                 a blocked frame read must fit in the graceful shutdown",
                args.read_timeout_ms, args.shutdown_timeout_ms
            )));
        }
        if args.hub_capacity.get() > MAX_HUB_CAPACITY {
            return Err(ConfigError::Invalid(format!(
                "--hub-capacity must not exceed {MAX_HUB_CAPACITY} frames"
            )));
        }
        if args.max_clients.get() > MAX_CLIENTS_LIMIT {
            return Err(ConfigError::Invalid(format!(
                "--max-clients must not exceed {MAX_CLIENTS_LIMIT}"
            )));
        }
        if args.reconnect_max_ms < args.reconnect_initial_ms {
            return Err(ConfigError::Invalid(
                "--reconnect-max-ms must be greater than or equal to --reconnect-initial-ms".into(),
            ));
        }
        Ok(Self {
            capture: CaptureSettings {
                kind: args.source.into(),
                device: DevicePath::new(args.device),
                resolution: args.resolution,
                fps,
                format,
                encoding: EncodingSettings {
                    quality,
                    white_balance: args.white_balance.into(),
                },
                read_timeout,
            },
            bind,
            max_clients: args.max_clients,
            hub_capacity: args.hub_capacity,
            reconnect_initial: Duration::from_millis(args.reconnect_initial_ms),
            reconnect_max: Duration::from_millis(args.reconnect_max_ms),
            idle_timeout: Duration::from_millis(args.idle_timeout_ms),
            shutdown_timeout: Duration::from_millis(args.shutdown_timeout_ms),
            max_corrupted_frames: args.max_corrupted_frames,
            banner: !args.no_banner,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::SourceKind;

    /// Les tests reposent sur l'absence de variables `CAMSTREAM_*` dans
    /// l'environnement (clap les lit) : on le signale clairement si ce n'est pas le cas.
    fn assert_clean_env() {
        let polluted: Vec<_> = std::env::vars()
            .map(|(key, _)| key)
            .filter(|key| key.starts_with("CAMSTREAM_"))
            .collect();
        assert!(
            polluted.is_empty(),
            "unset {polluted:?} to run config tests"
        );
    }

    fn serve(args: &[&str]) -> Result<AppConfig, ConfigError> {
        assert_clean_env();
        match Command::try_parse_from(std::iter::once("camstream").chain(args.iter().copied()))? {
            Command::Serve(config) => Ok(config),
            Command::Healthcheck(_) => panic!("expected serve"),
        }
    }

    #[test]
    fn banner_is_shown_unless_disabled() {
        assert!(serve(&[]).unwrap().banner);
        assert!(!serve(&["--no-banner"]).unwrap().banner);
    }

    #[test]
    fn defaults_match_the_documentation() {
        let config = serve(&[]).unwrap();
        assert_eq!(config.bind.to_string(), "127.0.0.1:8080");
        assert_eq!(config.capture.kind, SourceKind::V4l2);
        assert_eq!(config.capture.device.to_string(), "/dev/video0");
        assert_eq!(config.capture.resolution.to_string(), "640x480");
        assert_eq!(config.capture.fps.get(), 15);
        assert_eq!(config.capture.format, FormatPreference::Auto);
        assert_eq!(config.capture.encoding.quality.get(), 80);
        assert_eq!(
            config.capture.encoding.white_balance,
            crate::domain::WhiteBalance::GrayWorld
        );
        assert_eq!(config.capture.read_timeout, Duration::from_millis(2000));
        assert_eq!(config.idle_timeout, Duration::from_secs(10));
        assert_eq!(config.max_clients.get(), 10);
        assert_eq!(config.hub_capacity.get(), 4);
        assert_eq!(config.reconnect_initial, Duration::from_secs(1));
        assert_eq!(config.reconnect_max, Duration::from_secs(30));
        assert_eq!(config.shutdown_timeout, Duration::from_secs(5));
        assert_eq!(config.max_corrupted_frames.get(), 30);
    }

    #[test]
    fn options_are_parsed() {
        let config = serve(&[
            "--source",
            "synthetic",
            "--device",
            "/dev/video2",
            "--resolution",
            "1280x720",
            "--fps",
            "30",
            "--format",
            "mjpeg",
            "--bind",
            "0.0.0.0",
            "--port",
            "9000",
            "--idle-timeout-ms",
            "0",
            "--max-clients",
            "3",
            "--hub-capacity",
            "8",
        ])
        .unwrap();
        assert_eq!(config.capture.kind, SourceKind::Synthetic);
        assert_eq!(config.capture.device.to_string(), "/dev/video2");
        assert_eq!(config.capture.resolution.to_string(), "1280x720");
        assert_eq!(config.capture.fps.get(), 30);
        assert_eq!(config.capture.format, FormatPreference::Mjpeg);
        assert_eq!(config.bind.to_string(), "0.0.0.0:9000");
        assert_eq!(config.idle_timeout, Duration::ZERO);
        assert_eq!(config.max_clients.get(), 3);
        assert_eq!(config.hub_capacity.get(), 8);
    }

    #[test]
    fn raw_formats_and_encoding_are_configurable() {
        for (value, expected) in [
            ("yuyv", FormatPreference::Yuyv),
            ("bayer", FormatPreference::Bayer),
            ("auto", FormatPreference::Auto),
        ] {
            assert_eq!(
                serve(&["--format", value]).unwrap().capture.format,
                expected
            );
        }
        let config = serve(&["--jpeg-quality", "65", "--white-balance", "off"]).unwrap();
        assert_eq!(config.capture.encoding.quality.get(), 65);
        assert_eq!(
            config.capture.encoding.white_balance,
            crate::domain::WhiteBalance::Off
        );
        for quality in ["0", "101", "high"] {
            assert!(
                matches!(
                    serve(&["--jpeg-quality", quality]),
                    Err(ConfigError::Parse(_))
                ),
                "{quality}"
            );
        }
    }

    #[test]
    fn malformed_values_are_parse_errors() {
        for args in [
            &["--resolution", "640"][..],
            &["--fps", "0"],
            &["--fps", "500"],
            &["--max-clients", "0"],
            &["--source", "usb"],
            &["--bind", "localhost"],
            &["--read-timeout-ms", "0"],
        ] {
            assert!(
                matches!(serve(args), Err(ConfigError::Parse(_))),
                "{args:?}"
            );
        }
    }

    #[test]
    fn read_timeout_must_fit_in_the_shutdown_timeout() {
        for args in [
            &["--read-timeout-ms", "5000"][..],
            &["--read-timeout-ms", "6000"],
            &["--read-timeout-ms", "2000", "--shutdown-timeout-ms", "2000"],
        ] {
            match serve(args) {
                Err(ConfigError::Invalid(message)) => {
                    assert!(message.contains("--shutdown-timeout-ms"), "{message}");
                }
                other => panic!("{args:?}: unexpected {other:?}"),
            }
        }
        let config = serve(&["--read-timeout-ms", "4999"]).unwrap();
        assert_eq!(config.capture.read_timeout, Duration::from_millis(4999));
    }

    #[test]
    fn corrupted_frame_threshold_is_configurable() {
        let config = serve(&["--max-corrupted-frames", "5"]).unwrap();
        assert_eq!(config.max_corrupted_frames.get(), 5);
        assert!(matches!(
            serve(&["--max-corrupted-frames", "0"]),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn inconsistent_values_are_rejected() {
        for args in [
            &["--fps", "1", "--read-timeout-ms", "1000"][..],
            &["--hub-capacity", "65"],
            &["--max-clients", "1001"],
            // Valeur qui faisait paniquer `Semaphore::new` avant la borne.
            &["--max-clients", "18446744073709551615"],
            &[
                "--reconnect-initial-ms",
                "5000",
                "--reconnect-max-ms",
                "1000",
            ],
        ] {
            assert!(
                matches!(serve(args), Err(ConfigError::Invalid(_))),
                "{args:?}"
            );
        }
    }

    #[test]
    fn healthcheck_uses_the_listen_options() {
        assert_clean_env();
        let command =
            Command::try_parse_from(["camstream", "healthcheck", "--port", "9000"]).unwrap();
        assert_eq!(
            command,
            Command::Healthcheck(HealthcheckConfig {
                bind: "127.0.0.1:9000".parse().unwrap(),
                timeout: Duration::from_secs(2),
            })
        );
        let command =
            Command::try_parse_from(["camstream", "--bind", "0.0.0.0", "healthcheck"]).unwrap();
        assert!(matches!(command, Command::Healthcheck(c) if c.bind.to_string() == "0.0.0.0:8080"));
    }
}
