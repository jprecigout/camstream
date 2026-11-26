//! Erreurs du spike et classification des codes `errno` renvoyés par V4L2.
//!
//! La classification sert à vérifier sur matériel réel la table de traduction prévue
//! par l'architecture (`ENODEV` → déconnexion, `EACCES`/`EPERM` → droits, etc.).

use std::io;
use std::path::PathBuf;

use serde::Serialize;

/// Erreurs typées du spike.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    /// Échec d'ouverture du périphérique.
    #[error("cannot open video device {path}: {kind}", path = path.display())]
    Open {
        /// Chemin demandé.
        path: PathBuf,
        /// Classification de l'erreur système.
        kind: ErrnoKind,
        /// Erreur d'origine.
        #[source]
        source: io::Error,
    },
    /// Échec d'un ioctl de requête ou de configuration.
    #[error("V4L2 request failed ({operation}): {kind}")]
    Ioctl {
        /// Opération en cours (ex. « set_format »).
        operation: &'static str,
        /// Classification de l'erreur système.
        kind: ErrnoKind,
        /// Erreur d'origine.
        #[source]
        source: io::Error,
    },
    /// La webcam n'a pas accepté le MJPEG (obligatoire en v1, ADR 0006/0007).
    #[error("device does not deliver MJPEG (negotiated {negotiated})")]
    MjpegUnsupported {
        /// FourCC réellement négocié.
        negotiated: String,
    },
    /// Écriture de la trame d'exemple impossible.
    #[error("cannot write frame to {path}", path = path.display())]
    SaveFrame {
        /// Fichier de destination.
        path: PathBuf,
        /// Erreur d'origine.
        #[source]
        source: io::Error,
    },
}

impl ProbeError {
    /// Construit une erreur d'ioctl en classant son `errno`.
    pub fn ioctl(operation: &'static str, source: io::Error) -> Self {
        Self::Ioctl {
            operation,
            kind: ErrnoKind::classify(&source),
            source,
        }
    }
}

/// Catégorie d'une erreur système V4L2, avec le nom symbolique de l'`errno`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrnoKind {
    /// Périphérique absent (`ENOENT`).
    Enoent,
    /// Droits insuffisants (`EACCES`), ex. utilisateur hors du groupe `video`.
    Eacces,
    /// Opération interdite (`EPERM`), ex. refus du cgroup de périphériques Docker.
    Eperm,
    /// Périphérique disparu (`ENODEV`), ex. câble USB retiré.
    Enodev,
    /// Pas de périphérique pour ce nœud (`ENXIO`).
    Enxio,
    /// Argument invalide (`EINVAL`), attendu après un `TimedOut` avec `v4l` 0.14.
    Einval,
    /// Périphérique occupé (`EBUSY`), ex. déjà utilisé par un autre processus.
    Ebusy,
    /// Délai dépassé côté `v4l` (`poll` sans trame : `io::ErrorKind::TimedOut`).
    TimedOut,
    /// Autre erreur.
    Other,
}

impl ErrnoKind {
    /// Classe une erreur d'entrée/sortie.
    ///
    /// `TimedOut` est testé en premier : `v4l` le fabrique sans `errno` (code brut absent).
    pub fn classify(error: &io::Error) -> Self {
        if error.kind() == io::ErrorKind::TimedOut && error.raw_os_error().is_none() {
            return Self::TimedOut;
        }
        match error.raw_os_error() {
            Some(libc::ENOENT) => Self::Enoent,
            Some(libc::EACCES) => Self::Eacces,
            Some(libc::EPERM) => Self::Eperm,
            Some(libc::ENODEV) => Self::Enodev,
            Some(libc::ENXIO) => Self::Enxio,
            Some(libc::EINVAL) => Self::Einval,
            Some(libc::EBUSY) => Self::Ebusy,
            Some(libc::ETIMEDOUT) => Self::TimedOut,
            _ => Self::Other,
        }
    }

    /// Nom symbolique, pour l'affichage texte.
    pub fn name(self) -> &'static str {
        match self {
            Self::Enoent => "ENOENT",
            Self::Eacces => "EACCES",
            Self::Eperm => "EPERM",
            Self::Enodev => "ENODEV",
            Self::Enxio => "ENXIO",
            Self::Einval => "EINVAL",
            Self::Ebusy => "EBUSY",
            Self::TimedOut => "TIMED_OUT",
            Self::Other => "OTHER",
        }
    }
}

impl std::fmt::Display for ErrnoKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_known_errno_values() {
        let cases = [
            (libc::ENOENT, ErrnoKind::Enoent),
            (libc::EACCES, ErrnoKind::Eacces),
            (libc::EPERM, ErrnoKind::Eperm),
            (libc::ENODEV, ErrnoKind::Enodev),
            (libc::ENXIO, ErrnoKind::Enxio),
            (libc::EINVAL, ErrnoKind::Einval),
            (libc::EBUSY, ErrnoKind::Ebusy),
            (libc::EIO, ErrnoKind::Other),
        ];
        for (code, expected) in cases {
            let error = io::Error::from_raw_os_error(code);
            assert_eq!(ErrnoKind::classify(&error), expected, "errno {code}");
        }
    }

    #[test]
    fn classifies_v4l_synthetic_timeout() {
        // Forme exacte produite par v4l 0.14 dans `dequeue` quand `poll` expire.
        let error = io::Error::new(io::ErrorKind::TimedOut, "VIDIOC_DQBUF");
        assert_eq!(ErrnoKind::classify(&error), ErrnoKind::TimedOut);
    }

    #[test]
    fn ioctl_error_message_names_operation_and_errno() {
        let error = ProbeError::ioctl("set_format", io::Error::from_raw_os_error(libc::EBUSY));
        assert_eq!(error.to_string(), "V4L2 request failed (set_format): EBUSY");
    }
}
