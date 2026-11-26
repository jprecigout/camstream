//! Appels à la crate `v4l` protégés contre ses paniques.
//!
//! `v4l` 0.14 panique sur des données inattendues du pilote, hors de tout `Drop` :
//! `Capabilities::from` (`str::from_utf8(..).unwrap()` sur un nom de carte tronqué au
//! milieu d'un caractère UTF-8), `Format::from` (`expect` sur un espace colorimétrique
//! inconnu d'un noyau récent), `v4l2::open` (`CString::new(..).unwrap()`), ou la
//! destruction de l'`Arena` quand `MmapStream::with_buffers` échoue en cours
//! d'allocation. Sans protection, la panique tue le thread de capture : `Failed`,
//! code 1, et boucle de redémarrage Docker. Ici, elle devient une
//! [`CaptureError::DriverFault`] (reprise `Reopen`, voir architecture § 7.2).

use std::any::Any;
use std::panic::{self, AssertUnwindSafe};

use crate::domain::CaptureError;

/// Exécute `call` (appel à `v4l`) en interceptant une éventuelle panique.
///
/// `operation` nomme l'appel dans l'erreur et les logs. L'état éventuellement
/// incohérent laissé par la panique n'est jamais réutilisé : la source en cours de
/// construction est abandonnée et l'appelant la recrée plus tard (`Reopen`), d'où
/// l'usage justifié de `AssertUnwindSafe`.
pub(crate) fn guarded<T>(
    operation: &'static str,
    call: impl FnOnce() -> Result<T, CaptureError>,
) -> Result<T, CaptureError> {
    panic::catch_unwind(AssertUnwindSafe(call)).unwrap_or_else(|payload| {
        let message = panic_message(&*payload);
        // Le message complet a déjà été écrit par le gestionnaire de panique par défaut.
        tracing::error!(operation, %message, "v4l panicked; the camera will be reopened");
        Err(CaptureError::DriverFault { operation, message })
    })
}

/// Extrait le message d'une panique (`&str` ou `String`), sinon un texte générique.
///
/// Duplication volontaire de la fonction homonyme de `stream::supervisor` : l'adaptateur
/// `capture` ne doit pas dépendre de `stream` (ADR 0001), et ces quelques lignes ne
/// justifient pas un module utilitaire partagé.
fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Recovery;

    #[test]
    fn results_pass_through() {
        assert_eq!(guarded("noop", || Ok(7)).unwrap(), 7);
        let error = guarded::<()>("noop", || Err(CaptureError::Timeout)).unwrap_err();
        assert!(matches!(error, CaptureError::Timeout));
    }

    #[test]
    fn a_panic_becomes_a_recoverable_driver_fault() {
        // Reproduit la panique de `Capabilities::from` sur un nom tronqué en plein UTF-8.
        let truncated: &[u8] = std::hint::black_box(&[b'C', b'a', b'm', 0xC3]);
        let error = guarded("query_caps", || {
            let name = std::str::from_utf8(truncated).unwrap();
            Ok(name.len())
        })
        .unwrap_err();
        match &error {
            CaptureError::DriverFault { operation, message } => {
                assert_eq!(*operation, "query_caps");
                assert!(message.contains("Utf8Error"), "{message}");
            }
            other => panic!("unexpected error {other:?}"),
        }
        assert_eq!(error.recovery(), Recovery::Reopen);
        assert!(error
            .to_string()
            .starts_with("video driver library failed during query_caps"));
    }

    #[test]
    fn string_and_str_payloads_are_reported() {
        let error = guarded::<()>("set_format", || panic!("Invalid colorspace")).unwrap_err();
        assert!(error.to_string().ends_with("Invalid colorspace"));
        let error = guarded::<()>("set_format", || panic!("code {}", 42)).unwrap_err();
        assert!(error.to_string().ends_with("code 42"));
    }
}
