# 0003 — Choix des crates (runtime, HTTP, V4L2, erreurs, configuration, logs)

- Statut : Proposé
- Date : 2026-11-26

## Contexte

`CLAUDE.md` impose des dépendances **minimales, justifiées, auditées** et compatibles ARM
(cible unique `aarch64`, Raspberry Pi OS 64 bits ; voir [ADR 0005](0005-image-docker-et-compilation-arm64.md)). L'image Docker est construite sur une machine de build
x86_64 pour une cible `linux/arm64` (voir [ADR 0005](0005-image-docker-et-compilation-arm64.md)) :
les crates qui exigent des outils natifs au build (bindgen/libclang, bibliothèques C) ont un
coût de cross-compilation à prendre en compte.

## Options envisagées

### Accès V4L2 (point le plus délicat)

| Crate | Avantages | Inconvénients |
|---|---|---|
| **`v4l`** (libv4l-rs) | La plus utilisée ; API sûre (`Device`, `Capture`, `MmapStream`) ; capture mmap (sans copie noyau → espace utilisateur) ; énumération des formats ; fonctionne sur Raspberry Pi | Feature par défaut `v4l2` reposant sur `v4l2-sys-mit` généré par **bindgen** → libclang et en-têtes noyau requis au build, à configurer en cross-compilation ; rythme de maintenance modéré |
| `nokhwa` | Haut niveau, multiplateforme | Utilise `v4l` en dessous sous Linux ; tire `image` et des décodeurs inutiles ; abstraction multiplateforme sans intérêt ici (cible Linux uniquement) |
| `linuxvideo` | 100 % Rust, **sans bindgen** ni en-têtes C : cross-compilation triviale | Plus jeune et moins utilisée ; API moins éprouvée ; documentation plus mince |
| `v4l2r` | Très complet (DMABUF, codecs stateful) | Orientée codecs matériels (ChromeOS) ; bindgen ; complexité inutile pour une capture simple |
| `rscam` | API simple | Non maintenue depuis plusieurs années |
| ioctl directs (`nix`/`libc`) | Contrôle total, zéro dépendance lourde | Beaucoup de code `unsafe` (interdit sauf nécessité absolue) ; réinvente `v4l` |

### Runtime asynchrone et HTTP

| Option | Avantages | Inconvénients |
|---|---|---|
| **`tokio` + `axum`** | Standard de fait ; corps de réponse en streaming simple (`Body::from_stream`) ; arrêt gracieux intégré (`with_graceful_shutdown`) ; testable sans socket via `tower::ServiceExt::oneshot` ; compile sans souci sur ARM | Taille du binaire moyenne |
| `actix-web` | Performant | Runtime propre (acteurs), écosystème moins aligné sur `tokio::sync` |
| `hyper` seul | Plus léger | Routage, extraction et tests à écrire à la main |
| `tiny_http` (synchrone) | Minimal | Un thread par client ; pas de streaming async ; s'accorde mal avec `broadcast` |

### Erreurs, configuration, logs

- Erreurs : `thiserror` (modules) + `anyhow` (frontière `main`) — imposé par `CLAUDE.md`.
- Configuration : `clap` (derive + feature `env`) couvre CLI **et** variables d'environnement
  avec une seule déclaration ; alternatives écartées : `figment`/`config` (fichiers multiples,
  inutiles en v1), `envy` (pas de CLI).
- Logs : `tracing` + `tracing-subscriber` (`fmt`, `env-filter`) — imposé par `CLAUDE.md`.

## Décision

| Besoin | Crate | Features / remarques |
|---|---|---|
| Runtime | `tokio` | `rt-multi-thread`, `macros`, `net`, `signal`, `sync`, `time` |
| Arrêt coordonné | `tokio-util` | `CancellationToken` (utilisable depuis un thread synchrone via `is_cancelled()`) |
| HTTP | `axum` | features par défaut réduites au besoin (`http1`, `tokio`, `json`, `tracing`) |
| Flux du corps HTTP | `futures-util` | `stream::unfold` (déjà tiré par axum) |
| Trames sans copie | `bytes` | `Bytes` partagé entre clients |
| V4L2 | **`v4l`** | feature `v4l2` (pas de dépendance d'exécution à libv4l) ; derrière la feature Cargo `v4l2` de Camstream |
| Configuration | `clap` | `derive`, `env` |
| Sérialisation `/health` | `serde` (+ `serde_json` via axum) | `derive` ; utilisée **uniquement dans `server`** (DTO `HealthReport`), jamais dans `domain`/`stream` |
| Options de socket TCP | `socket2` 0.6 | feature **`all`** (requise pour `set_tcp_user_timeout` et `TcpKeepalive::with_retries`) ; `SO_KEEPALIVE` et `TCP_USER_TIMEOUT` sur les connexions acceptées. Déjà tirée par `tokio` (feature `net`, qui active aussi `all`) : la déclarer en dépendance directe avec la même version majeure n'ajoute pas de crate au graphe |
| Erreurs | `thiserror`, `anyhow` | `anyhow` uniquement dans `main.rs` / `app` |
| Logs | `tracing`, `tracing-subscriber` | `fmt`, `env-filter` |
| Encodage JPEG (formats bruts YUYV et Bayer) | `jpeg-encoder` 0.7 | 100 % Rust, sans dépendance ; toujours compilée (sans feature `simd`) depuis l'[ADR 0008](0008-formats-bruts-et-conversion-jpeg.md) |
| Codes `errno` (adaptateur V4L2) | `libc` | optionnelle, activée par la feature `v4l2` ; déjà tirée par `v4l` |
| Tests | `tower` (`util`), `http-body-util`, `hyper` (`client`, `http1`), `hyper-util` (`tokio`), `serde_json`, `jpeg-decoder` | dev-dependencies uniquement ; toutes déjà dans le graphe via axum (le client HTTP/1.1 de hyper lit le flux multipart morceau par morceau), sauf `jpeg-decoder` (ADR 0008 : décodage des JPEG produits par le convertisseur) |

Le choix de `v4l` est conditionné à un **spike de cross-compilation** (bindgen vers
`aarch64-unknown-linux-gnu`) en tout début d'implémentation. En cas d'échec ou de coût
excessif, le repli est `linuxvideo` : l'adaptateur étant isolé derrière `FrameSource`, ce
changement n'impacte que `src/capture/v4l2/`.

> **Avancement du spike (2026-11-26)** : la **cross-compilation de `v4l` 0.14 vers
> `aarch64-unknown-linux-gnu` est validée** (bindgen 0.65 via `v4l2-sys-mit` 0.3, clang 18,
> en-têtes `linux-libc-dev-arm64-cross`), hors Docker ; binaire de 1,7 Mio sans dépendance
> d'exécution V4L2. Restent à mesurer sur le Raspberry Pi le comportement de la crate avec une
> webcam réelle. Le statut reste « Proposé » jusqu'à ces mesures :
> [spike 0001](../spikes/0001-v4l.md).

> **Complément (implémentation, 2026-11-26)** : versions retenues dans `Cargo.lock` :
> `tokio` 1.54, `axum` 0.8.9, `socket2` 0.6.5, `clap` 4.6, `v4l` 0.14.0, `thiserror` 2.
> `rust-version` = **1.88** : imposé par `home` 0.5.12, dépendance de build de bindgen via
> `v4l2-sys-mit` (1.85 suffirait sans la feature `v4l2`). Pas de feature `yuyv` ni de
> `jpeg-encoder` en v1 (phase 2).

> **Complément (formats bruts, 2026-11-27)** : `jpeg-encoder` **0.7.1** est adoptée pour
> l'encodage des formats bruts (YUYV, Bayer) : MIT/Apache-2.0 **et IJG** (mention
> d'attribution ajoutée au README), aucune dépendance, `forbid(unsafe_code)` sans la feature
> `simd`, `rust-version` 1.87, dernière version du 2026-07-27. Elle n'est plus optionnelle
> (pas de feature `yuyv`) : la source synthétique brute l'utilise aussi sans la feature
> `v4l2`. Elle est compilée en `opt-level = 3` même en profil `dev`
> (`[profile.dev.package.jpeg-encoder]`) pour garder les tests de bout en bout rapides.
> `jpeg-decoder` 0.3 (MIT/Apache-2.0) est ajoutée en dépendance de test. Mesures et
> justification : [ADR 0008](0008-formats-bruts-et-conversion-jpeg.md).

## Conséquences

- L'étape de build Docker doit fournir `clang`/`libclang-dev` et les en-têtes noyau de la cible
  (`linux-libc-dev-arm64-cross`) pour bindgen.
- Toutes les crates retenues sont 100 % Rust à l'exception de la génération de bindings V4L2 au
  build : pas de bibliothèque C à embarquer dans l'image finale (hors glibc).
- `cargo deny` / `cargo audit` sont à intégrer à la CI (licences, avis de sécurité).
- Le cœur (`domain`, `stream`) ne dépend que de `bytes`, `thiserror`, `tokio` (`sync`) et
  `tokio-util` : la représentation JSON de l'état est un DTO de l'adaptateur HTTP
  (`server::health::HealthReport`, construit par `From<&SourceStatus>`). Ainsi, renommer une
  variante interne ne casse pas le contrat JSON, et le cœur n'acquiert pas de dépendance de
  sérialisation pour un unique endpoint.
- Encodage des formats bruts → JPEG : `jpeg-encoder` est retenu pour la simplicité de build ;
  si ses performances sont insuffisantes sur Pi, `turbojpeg` (libjpeg-turbo, optimisé NEON)
  sera réévalué par ADR (estimations dans l'ADR 0008).
