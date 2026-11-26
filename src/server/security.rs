//! En-têtes de sécurité HTTP ajoutés à **toutes** les réponses (ADR 0007).
//!
//! La v1 n'a ni authentification ni TLS ; ces en-têtes limitent ce qu'un navigateur
//! accepte de faire avec les réponses de Camstream (interprétation d'un type MIME
//! deviné, inclusion dans un cadre d'un autre site, scripts injectés, fuite de
//! l'adresse du Pi dans l'en-tête `Referer`).
//!
//! Implémente le pattern **Decorator** : [`add_security_headers`] enveloppe le routeur
//! entier (`map_response`) et enrichit chaque réponse sans que les handlers le sachent.

use axum::http::header::{
    HeaderName, HeaderValue, CONTENT_SECURITY_POLICY, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS,
    X_FRAME_OPTIONS,
};
use axum::response::Response;

/// Politique CSP de la page d'accueil et de toutes les réponses.
///
/// Tout est interdit par défaut ; seules les ressources de la même origine
/// nécessaires à la page sont permises : `/camstream.js` (pas de script en ligne),
/// `/camstream.css` (pas de style en ligne), `/stream.mjpg` (image) ; aucune requête
/// `fetch`, aucun cadre, aucun formulaire.
pub const CONTENT_SECURITY_POLICY_VALUE: &str = "default-src 'none'; script-src 'self'; \
     style-src 'self'; img-src 'self'; base-uri 'none'; form-action 'none'; \
     frame-ancestors 'none'";

/// Ajoute les en-têtes de sécurité à une réponse (middleware `map_response`), sans
/// écraser un en-tête déjà défini par un handler (pattern **Decorator**).
pub(crate) async fn add_security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    let mut add = |name: HeaderName, value: &'static str| {
        headers
            .entry(name)
            .or_insert(HeaderValue::from_static(value));
    };
    // Le navigateur n'interprète jamais une réponse selon un type deviné.
    add(X_CONTENT_TYPE_OPTIONS, "nosniff");
    add(CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY_VALUE);
    // Doublon de `frame-ancestors 'none'` pour les navigateurs anciens.
    add(X_FRAME_OPTIONS, "DENY");
    // L'adresse du Pi ne fuit pas vers un site tiers.
    add(REFERRER_POLICY, "no-referrer");
    response
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    use super::*;

    #[tokio::test]
    async fn every_header_is_added_without_overriding() {
        let response = (StatusCode::OK, [(REFERRER_POLICY, "same-origin")], "body").into_response();
        let response = add_security_headers(response).await;
        let headers = response.headers();
        assert_eq!(headers[X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(headers[X_FRAME_OPTIONS], "DENY");
        assert_eq!(
            headers[CONTENT_SECURITY_POLICY],
            CONTENT_SECURITY_POLICY_VALUE
        );
        assert_eq!(headers[REFERRER_POLICY], "same-origin");
    }

    #[test]
    fn policy_forbids_inline_code_and_framing() {
        assert!(!CONTENT_SECURITY_POLICY_VALUE.contains("unsafe-inline"));
        assert!(CONTENT_SECURITY_POLICY_VALUE.starts_with("default-src 'none'"));
        assert!(CONTENT_SECURITY_POLICY_VALUE.contains("frame-ancestors 'none'"));
    }
}
