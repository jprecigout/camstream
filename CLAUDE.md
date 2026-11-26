# CLAUDE.md — Camstream

Ce fichier définit les règles du projet pour Claude et pour tout contributeur.
Elles sont **obligatoires**.

## 1. Présentation du projet

**Camstream** est une application Rust qui capture le flux vidéo d'une webcam USB
et le diffuse dans un navigateur web.

- **Cible** : Raspberry Pi (architecture ARM64 / `aarch64`, éventuellement ARMv7).
- **Déploiement** : conteneur Docker (accès à la webcam via `--device /dev/video0`).
- **Capture** : V4L2 (Video4Linux2), formats MJPEG en priorité (évite un ré-encodage
  coûteux sur le Raspberry), YUYV en repli.
- **Diffusion** : serveur HTTP embarqué. Point de départ recommandé : flux
  MJPEG (`multipart/x-mixed-replace`), lisible nativement par tout navigateur ;
  évolutions possibles : WebSocket, WebRTC.
- **Dépôt** : https://github.com/jprecigout/camstream

## 2. Langues

| Élément | Langue |
|---|---|
| Code (identifiants, noms de types, fonctions, variables, modules, crates) | **Anglais** |
| Messages de log et d'erreur techniques | Anglais |
| Commentaires dans le code (`//`, `///`, `//!`) | **Français — obligatoire** |
| Documentation (`docs/`, README, ADR) | Français |
| Messages de commit (description) | Français |
| Revues de code, échanges, issues, PR | Français |

Règles sur les commentaires :

- Chaque élément public (`pub`) doit avoir un commentaire de documentation `///` en français.
- Chaque module doit commencer par un commentaire `//!` en français décrivant son rôle.
- Les blocs non triviaux sont commentés en français : expliquer le **pourquoi**, pas le quoi.
- Tout bloc `unsafe` est précédé d'un commentaire `// SÉCURITÉ :` justifiant sa validité.

```rust
//! Module de capture vidéo depuis une webcam V4L2.

/// Source de trames vidéo, indépendante du matériel.
///
/// Implémente le pattern **Strategy** : le serveur ne connaît que ce trait,
/// ce qui permet de remplacer la webcam par une source de test.
pub trait FrameSource: Send {
    /// Lit la prochaine trame disponible.
    fn next_frame(&mut self) -> Result<Frame, CaptureError>;
}
```

## 3. Bonnes pratiques Rust

- Respecter les [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/) et les idiomes du langage.
- **Formatage** : `cargo fmt` (rustfmt) obligatoire. Un fichier `rustfmt.toml` à la racine fait référence.
- **Lint** : `cargo clippy --all-targets --all-features -- -D warnings` doit passer sans avertissement.
- **Tests** : `cargo test` doit passer. Tests unitaires dans le module (`#[cfg(test)]`), tests d'intégration dans `tests/`.
- **Gestion d'erreurs** :
  - `thiserror` pour les erreurs typées des bibliothèques/modules ;
  - `anyhow` uniquement à la frontière applicative (`main`) ;
  - pas de `unwrap()` / `expect()` dans le code de production (tolérés dans les tests, ou avec un commentaire justifiant l'invariant).
- **Sécurité mémoire** : `unsafe` interdit sauf nécessité absolue, isolé et justifié (`#![deny(unsafe_op_in_unsafe_fn)]`).
- **Asynchrone** : runtime `tokio` ; ne jamais bloquer le runtime (capture V4L2 bloquante → thread dédié ou `spawn_blocking`).
- **Concurrence** : préférer le passage de messages (`tokio::sync::broadcast`, `watch`, `mpsc`) au partage d'état sous verrou.
- **Types** : utiliser le système de types (newtypes, enums, `Option`/`Result`) plutôt que des valeurs magiques.
- **Configuration** : via variables d'environnement et/ou fichier, jamais en dur ; valeurs par défaut documentées.
- **Logs** : `tracing` (niveaux adaptés, pas de `println!` en production).
- **Dépendances** : minimales, justifiées, auditées (`cargo audit`, `cargo deny` recommandés). Penser à la compatibilité ARM.
- **Performance** : le Raspberry est contraint ; éviter les copies inutiles (`bytes::Bytes`, partage par `Arc`), éviter le ré-encodage.

## 4. Architecture : découplage et design patterns

### Découplage (prioritaire)

- Séparer clairement les responsabilités : **capture**, **distribution des trames**, **transport HTTP**, **configuration**.
- Les modules communiquent via des **traits** (abstractions) et non via des types concrets.
- Inversion de dépendances : le cœur métier ne dépend ni de V4L2 ni du framework HTTP.
- Injection des dépendances par construction (constructeurs / builders), pas de singletons globaux mutables.
- Toute dépendance matérielle doit pouvoir être remplacée par une implémentation de test (mock / source synthétique).

### Design patterns

L'utilisation des design patterns est **favorisée** lorsqu'elle apporte de la clarté ou du découplage
(sans sur-ingénierie). Exemples pertinents pour ce projet :

| Pattern | Usage envisagé |
|---|---|
| **Strategy** | Trait `FrameSource` (webcam V4L2, source de test, fichier) |
| **Observer / Publish-Subscribe** | Diffusion des trames vers N clients navigateur (`broadcast`) |
| **Builder** | Construction de la configuration et du serveur |
| **Adapter** | Encapsulation de la crate V4L2 derrière nos propres traits |
| **Factory** | Création de la source vidéo selon la configuration |
| **Newtype** | Types forts (`Resolution`, `Fps`, `DevicePath`) |
| **RAII** | Libération automatique du périphérique et des buffers |
| **Typestate** | États du périphérique (ouvert / configuré / en streaming) vérifiés à la compilation |

Obligations :

- Chaque pattern utilisé est **nommé dans le commentaire de doc** du type concerné (ex. « Implémente le pattern **Strategy** »).
- Chaque pattern utilisé est **documenté dans `docs/design-patterns.md`** : nom du pattern, problème résolu, où il est utilisé (fichier/type), diagramme Mermaid.

## 5. Documentation

- Toute la documentation est rédigée **en français** dans le dossier **`docs/`**.
- Structure attendue :
  - `docs/architecture.md` — vue d'ensemble, composants, flux de données ;
  - `docs/design-patterns.md` — catalogue des patterns utilisés ;
  - `docs/deploiement.md` — Docker, Raspberry Pi, accès webcam ;
  - `docs/adr/NNNN-titre.md` — décisions d'architecture (Architecture Decision Records).
- **Tous les diagrammes sont faits en Mermaid** (blocs ` ```mermaid `) : architecture, séquence, classes, états, déploiement. Pas d'images de diagrammes.
- La documentation est mise à jour **dans le même commit** que le code qui la rend obsolète.

```mermaid
flowchart LR
    Cam[Webcam USB] -->|V4L2 / MJPEG| Capture[Capture]
    Capture -->|Trames| Hub[Diffuseur de trames]
    Hub --> HTTP[Serveur HTTP]
    HTTP -->|multipart MJPEG| Nav[Navigateur]
```

## 6. Docker et Raspberry Pi

- `Dockerfile` **multi-étapes** : étape de build Rust, image finale minimale (`debian:bookworm-slim` ou distroless).
- Build multi-architecture via `docker buildx` (`linux/arm64`, et `linux/arm/v7` si besoin).
- Le conteneur s'exécute en **utilisateur non root** (membre du groupe `video`).
- Accès webcam : `--device /dev/video0:/dev/video0` ; jamais `--privileged`.
- Exposer un endpoint de santé (`/health`) utilisé par `HEALTHCHECK`.
- Fournir un `docker-compose.yml` d'exemple.

## 7. Conventional Commits

Tous les commits respectent la spécification [Conventional Commits 1.0.0](https://www.conventionalcommits.org/fr/v1.0.0/).

```
<type>(<portée optionnelle>): <description en français, à l'impératif, sans point final>

[corps optionnel en français]

[pied optionnel : BREAKING CHANGE: ..., Refs: #12]
```

Types autorisés :

| Type | Usage |
|---|---|
| `feat` | Nouvelle fonctionnalité |
| `fix` | Correction de bug |
| `docs` | Documentation uniquement |
| `style` | Formatage, sans changement de logique |
| `refactor` | Restructuration sans changement de comportement |
| `perf` | Amélioration de performance |
| `test` | Ajout ou correction de tests |
| `build` | Système de build, dépendances, Docker |
| `ci` | Intégration continue |
| `chore` | Maintenance diverse |
| `revert` | Annulation d'un commit |

Portées suggérées : `capture`, `server`, `stream`, `config`, `docker`, `docs`.

Exemples :

```
feat(capture): ajouter la capture MJPEG via V4L2
fix(server): fermer proprement le flux à la déconnexion du client
docs(architecture): ajouter le diagramme de séquence du streaming
feat(config)!: renommer la variable CAMSTREAM_DEVICE

BREAKING CHANGE: CAMSTREAM_DEVICE remplace VIDEO_DEVICE.
```

Un commit = un changement logique. Pas de commit « wip » sur `main`.

## 8. Processus avant chaque commit (obligatoire)

Aucun commit sans **revue de code préalable**. Ordre à respecter :

1. `cargo fmt --all` — formatage rustfmt.
2. `cargo clippy --all-targets --all-features -- -D warnings` — aucun avertissement.
3. `cargo test --all-features` — tous les tests passent.
4. **Revue de code** par l'agent `rust-senior-developer` (et par `software-architect` si le changement touche l'architecture, les interfaces entre modules ou introduit un pattern). La revue vérifie :
   - respect des règles de ce fichier (langues, commentaires français, patterns documentés) ;
   - bonnes pratiques Rust, gestion d'erreurs, absence de `unwrap` non justifié ;
   - découplage et testabilité ;
   - documentation `docs/` et diagrammes Mermaid à jour.
5. Corriger les remarques bloquantes, puis relancer les étapes 1 à 3.
6. Commit au format Conventional Commits.

Le travail se fait sur des branches (`feat/...`, `fix/...`), fusionnées dans `main` via Pull Request.

## 9. Agents disponibles

- **`software-architect`** (`.claude/agents/software-architect.md`) : conception, découpage en modules, choix des patterns, ADR, diagrammes Mermaid, revue d'architecture.
- **`rust-senior-developer`** (`.claude/agents/rust-senior-developer.md`) : implémentation, tests, optimisation, revue de code Rust.

Flux recommandé : l'architecte conçoit (ADR + diagrammes) → le développeur implémente → revue croisée → commit.

## 10. Commandes utiles

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo run -- --device /dev/video0 --port 8080

docker buildx build --platform linux/arm64 -t camstream:latest .
docker run --rm --device /dev/video0 -p 8080:8080 camstream:latest
```
