//! Bannière affichée au démarrage du serveur : nom en grandes lettres aux couleurs du
//! logo (dégradé violet → rose → orange) et résumé de la configuration utile
//! (version, source vidéo, adresse à ouvrir dans le navigateur).
//!
//! Le module ne fait que **produire du texte** : le choix de la sortie et des couleurs
//! appartient au binaire (`main`), ce qui garde le rendu testable.

use std::fmt::Write as _;
use std::net::SocketAddr;

use crate::config::AppConfig;
use crate::domain::SourceKind;

/// Nom en grandes lettres (police « ANSI Shadow »), toutes lignes de même largeur.
const ART: [&str; 6] = [
    " ██████╗ █████╗ ███╗   ███╗███████╗████████╗██████╗ ███████╗ █████╗ ███╗   ███╗",
    "██╔════╝██╔══██╗████╗ ████║██╔════╝╚══██╔══╝██╔══██╗██╔════╝██╔══██╗████╗ ████║",
    "██║     ███████║██╔████╔██║███████╗   ██║   ██████╔╝█████╗  ███████║██╔████╔██║",
    "██║     ██╔══██║██║╚██╔╝██║╚════██║   ██║   ██╔══██╗██╔══╝  ██╔══██║██║╚██╔╝██║",
    "╚██████╗██║  ██║██║ ╚═╝ ██║███████║   ██║   ██║  ██║███████╗██║  ██║██║ ╚═╝ ██║",
    " ╚═════╝╚═╝  ╚═╝╚═╝     ╚═╝╚══════╝   ╚═╝   ╚═╝  ╚═╝╚══════╝╚═╝  ╚═╝╚═╝     ╚═╝",
];

/// Couleur RVB 24 bits.
type Rgb = (u8, u8, u8);

/// Arrêts du dégradé du logo (`assets/logo/`) : position (0 à 1) et couleur.
const GRADIENT: [(f32, Rgb); 3] = [
    (0.0, (0x7B, 0x2F, 0xF7)),
    (0.55, (0xF7, 0x25, 0x85)),
    (1.0, (0xFF, 0x8C, 0x42)),
];

/// Gris atténué des libellés.
const DIM: Rgb = (0x9A, 0x94, 0xB0);

/// Rendu des couleurs de la bannière.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    /// Séquences ANSI 24 bits (terminal interactif).
    TrueColor,
    /// Texte brut, sans séquence d'échappement (`docker logs`, fichier, `NO_COLOR`).
    Plain,
}

/// Bannière de démarrage, construite à partir de la configuration validée.
///
/// ```
/// # use camstream::banner::{Banner, ColorMode};
/// # use camstream::config::Command;
/// let Command::Serve(config) = Command::try_parse_from(["camstream"]).expect("defaults") else {
///     unreachable!("no subcommand: serve")
/// };
/// let text = Banner::new(&config, "0.1.0").render(ColorMode::Plain);
/// assert!(text.contains("v0.1.0"));
/// ```
#[derive(Debug, Clone, Copy)]
pub struct Banner<'a> {
    config: &'a AppConfig,
    version: &'a str,
}

impl<'a> Banner<'a> {
    /// Prépare la bannière pour cette configuration et cette version du binaire.
    pub fn new(config: &'a AppConfig, version: &'a str) -> Self {
        Self { config, version }
    }

    /// Produit le texte de la bannière (terminé par une ligne vide).
    pub fn render(&self, mode: ColorMode) -> String {
        let mut out = String::with_capacity(2048);
        out.push('\n');
        for line in ART {
            push_gradient_line(&mut out, line, mode);
        }
        out.push('\n');

        let capture = &self.config.capture;
        let source = match capture.kind {
            SourceKind::V4l2 => format!("V4L2 webcam {}", capture.device),
            SourceKind::Synthetic => "synthetic test pattern".to_owned(),
        };
        let url = browser_url(self.config.bind);
        let rows = [
            ("version", format!("v{}", self.version)),
            (
                "source",
                // Valeurs **demandées** : le format effectif, négocié avec la caméra, est
                // journalisé à l'ouverture et exposé par `/health`.
                format!(
                    "{source} (requested {} @ {} fps, format {})",
                    capture.resolution, capture.fps, capture.format
                ),
            ),
            ("stream", url.clone()),
            ("health", format!("{url}health")),
        ];
        for (label, value) in rows {
            // Les écritures dans une `String` n'échouent jamais.
            let _ = writeln!(
                out,
                "  {}  {value}",
                paint(&format!("{label:<8}"), DIM, mode)
            );
        }
        out.push('\n');
        out
    }
}

/// URL à ouvrir dans le navigateur. Une écoute sur toutes les interfaces n'est pas une
/// adresse joignable : on invite alors à utiliser l'adresse du Pi.
fn browser_url(bind: SocketAddr) -> String {
    if bind.ip().is_unspecified() {
        format!("http://<raspberry-pi-ip>:{}/", bind.port())
    } else {
        format!("http://{bind}/")
    }
}

/// Ajoute une ligne du nom, chaque caractère coloré selon sa colonne.
fn push_gradient_line(out: &mut String, line: &str, mode: ColorMode) {
    out.push_str("  ");
    match mode {
        ColorMode::Plain => out.push_str(line),
        ColorMode::TrueColor => {
            let width = ART[0].chars().count().saturating_sub(1).max(1);
            for (column, ch) in line.chars().enumerate() {
                if ch == ' ' {
                    out.push(' ');
                } else {
                    // Conversion exacte : la largeur ne dépasse pas une centaine de colonnes.
                    let t = column as f32 / width as f32;
                    let (r, g, b) = gradient_at(t);
                    let _ = write!(out, "\x1b[38;2;{r};{g};{b}m{ch}");
                }
            }
            out.push_str("\x1b[0m");
        }
    }
    out.push('\n');
}

/// Couleur du dégradé à la position `t` (bornée à 0..=1), par interpolation linéaire.
fn gradient_at(t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let mut segment = (GRADIENT[0], GRADIENT[1]);
    for pair in GRADIENT.windows(2) {
        if t <= pair[1].0 {
            segment = (pair[0], pair[1]);
            break;
        }
    }
    let ((start, from), (end, to)) = segment;
    let local = (t - start) / (end - start);
    let mix = |a: u8, b: u8| {
        let value = f32::from(a) + (f32::from(b) - f32::from(a)) * local;
        // Valeur dans 0..=255 par construction (interpolation entre deux `u8`) ;
        // `as` sature de toute façon hors bornes.
        value.round() as u8
    };
    (mix(from.0, to.0), mix(from.1, to.1), mix(from.2, to.2))
}

/// Colore un texte d'une teinte unie (ou le laisse brut).
fn paint(text: &str, (r, g, b): Rgb, mode: ColorMode) -> String {
    match mode {
        ColorMode::Plain => text.to_owned(),
        ColorMode::TrueColor => format!("\x1b[38;2;{r};{g};{b}m{text}\x1b[0m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Command;

    fn config(args: &[&str]) -> AppConfig {
        let args = std::iter::once("camstream").chain(args.iter().copied());
        match Command::try_parse_from(args).expect("valid arguments") {
            Command::Serve(config) => config,
            Command::Healthcheck(_) => panic!("expected serve"),
        }
    }

    #[test]
    fn art_lines_share_the_same_width() {
        let width = ART[0].chars().count();
        assert!(ART.iter().all(|line| line.chars().count() == width));
    }

    #[test]
    fn plain_banner_has_no_escape_sequence_and_summarises_the_config() {
        let config = config(&["--source", "synthetic", "--port", "9000"]);
        let text = Banner::new(&config, "1.2.3").render(ColorMode::Plain);
        assert!(!text.contains('\x1b'));
        assert!(text.contains(ART[2]));
        assert!(text.contains("v1.2.3"));
        assert!(text.contains("synthetic test pattern (requested 640x480 @ 15 fps, format auto)"));
        assert!(text.contains("http://127.0.0.1:9000/"));
        assert!(text.contains("http://127.0.0.1:9000/health"));
    }

    #[test]
    fn v4l2_source_shows_the_device() {
        let config = config(&["--device", "/dev/video2"]);
        let text = Banner::new(&config, "1.0.0").render(ColorMode::Plain);
        assert!(text.contains("V4L2 webcam /dev/video2"));
    }

    #[test]
    fn unspecified_bind_address_asks_for_the_pi_address() {
        let config = config(&["--bind", "0.0.0.0"]);
        let text = Banner::new(&config, "1.0.0").render(ColorMode::Plain);
        assert!(text.contains("http://<raspberry-pi-ip>:8080/"));
    }

    #[test]
    fn colored_banner_resets_every_line() {
        let config = config(&[]);
        let text = Banner::new(&config, "1.0.0").render(ColorMode::TrueColor);
        assert!(text.contains("\x1b[38;2;"));
        for line in text
            .lines()
            .filter(|line| line.contains('█') || line.contains('═'))
        {
            assert!(line.ends_with("\x1b[0m"), "{line:?}");
        }
    }

    #[test]
    fn gradient_matches_the_logo_colors_at_its_stops() {
        assert_eq!(gradient_at(0.0), (0x7B, 0x2F, 0xF7));
        assert_eq!(gradient_at(0.55), (0xF7, 0x25, 0x85));
        assert_eq!(gradient_at(1.0), (0xFF, 0x8C, 0x42));
        assert_eq!(gradient_at(-1.0), gradient_at(0.0));
        assert_eq!(gradient_at(2.0), gradient_at(1.0));
    }
}
