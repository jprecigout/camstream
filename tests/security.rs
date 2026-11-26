//! En-têtes de sécurité HTTP (ADR 0007) : présents sur toutes les réponses, et page
//! d'accueil compatible avec la politique CSP stricte (aucun code en ligne).

mod support;

use camstream::server::security::CONTENT_SECURITY_POLICY_VALUE;
use hyper::{header, Method, StatusCode};
use support::{body_text, get, request, test_config, TestApp};

fn assert_security_headers(path: &str, headers: &hyper::HeaderMap) {
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff", "{path}");
    assert_eq!(
        headers[header::CONTENT_SECURITY_POLICY],
        CONTENT_SECURITY_POLICY_VALUE,
        "{path}"
    );
    assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY", "{path}");
    assert_eq!(headers[header::REFERRER_POLICY], "no-referrer", "{path}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_response_carries_the_security_headers() {
    let app = TestApp::start(test_config(), None).await;
    for (method, path, expected) in [
        (Method::GET, "/", StatusCode::OK),
        (Method::GET, "/camstream.js", StatusCode::OK),
        (Method::GET, "/camstream.css", StatusCode::OK),
        (Method::GET, "/health", StatusCode::OK),
        (Method::GET, "/stream.mjpg", StatusCode::OK),
        (Method::GET, "/nope", StatusCode::NOT_FOUND),
        (Method::POST, "/health", StatusCode::METHOD_NOT_ALLOWED),
    ] {
        let response = request(app.addr, method, path).await;
        assert_eq!(response.status(), expected, "{path}");
        assert_security_headers(path, response.headers());
    }
    assert_eq!(app.stop().await, camstream::app::Outcome::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_stream_clients_also_get_the_headers() {
    let mut config = test_config();
    config.max_clients = 1.try_into().unwrap();
    let app = TestApp::start(config, None).await;
    let _first = get(app.addr, "/stream.mjpg").await;
    let refused = get(app.addr, "/stream.mjpg").await;
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_security_headers("/stream.mjpg (503)", refused.headers());
    assert_eq!(app.stop().await, camstream::app::Outcome::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_page_works_under_the_policy() {
    let app = TestApp::start(test_config(), None).await;
    let page = body_text(get(app.addr, "/").await).await;
    // Ressources externes de même origine uniquement…
    assert!(page.contains(r#"<script src="/camstream.js" defer></script>"#));
    assert!(page.contains(r#"<link rel="stylesheet" href="/camstream.css">"#));
    assert!(page.contains(r#"src="/stream.mjpg""#));
    // … et aucun code en ligne, que `script-src 'self'` / `style-src 'self'` bloqueraient.
    assert_eq!(
        page.matches("<script").count(),
        1,
        "only the external script"
    );
    for forbidden in ["<style", " style=", " onload=", " onerror=", "javascript:"] {
        assert!(!page.contains(forbidden), "inline code: {forbidden}");
    }

    // `nosniff` exige le type MIME exact pour exécuter le script et appliquer le style.
    let script = get(app.addr, "/camstream.js").await;
    assert_eq!(
        script.headers()[header::CONTENT_TYPE],
        "text/javascript; charset=utf-8"
    );
    assert!(body_text(script)
        .await
        .contains("getElementById(\"stream\")"));
    let style = get(app.addr, "/camstream.css").await;
    assert_eq!(
        style.headers()[header::CONTENT_TYPE],
        "text/css; charset=utf-8"
    );
    assert!(body_text(style).await.contains("img.waiting"));
    assert_eq!(app.stop().await, camstream::app::Outcome::Stopped);
}
