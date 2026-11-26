# 0005 — Image Docker et compilation pour arm64

- Statut : Proposé
- Date : 2026-11-26

## Contexte

- Cible **unique** : Raspberry Pi sous Raspberry Pi OS 64 bits (`linux/arm64`). Les OS 32 bits
  (`linux/arm/v7`) et les modèles ARMv6 ne sont pas pris en charge (décision de l'utilisateur,
  2026-11-26) : une seule cible croisée à maintenir et à tester.
- `CLAUDE.md` impose un `Dockerfile` multi-étapes, une image finale minimale, un utilisateur
  non root membre du groupe `video`, l'accès à la webcam par `--device` (jamais
  `--privileged`) et un `HEALTHCHECK` sur `/health`.
- La crate `v4l` génère ses bindings avec bindgen au build ([ADR 0003](0003-choix-des-crates.md)).
- Compiler du Rust sur un Raspberry Pi est lent (plusieurs dizaines de minutes) et gourmand en
  mémoire.

## Options envisagées

### Mode de compilation

| Option | Avantages | Inconvénients |
|---|---|---|
| Build natif émulé (`buildx` + QEMU, étape de build en `linux/arm64`) | Dockerfile le plus simple ; aucune configuration de cross-compilation | **Très lent** (émulation ×5 à ×20) ; timeouts possibles en CI |
| **Cross-compilation dans l'étape de build** (`FROM --platform=$BUILDPLATFORM`, cible `aarch64-unknown-linux-gnu`) | Rapide (build natif x86_64) ; cache Cargo efficace | Configurer l'éditeur de liens croisé et bindgen (`--sysroot`, en-têtes noyau arm64) |
| Build sur le Raspberry lui-même | Aucun outil croisé | Lent, usure de la carte SD, non reproductible en CI |
| `cross-rs` hors Docker puis `COPY` du binaire | Outil éprouvé | Deux étapes distinctes, Dockerfile non autonome |

### Image finale

| Option | Avantages | Inconvénients |
|---|---|---|
| **`debian:bookworm-slim`** | glibc compatible avec la cible `-gnu` ; `useradd`/groupe `video` disponibles ; shell pour le diagnostic | ~75 Mio |
| `gcr.io/distroless/cc-debian12:nonroot` | Plus petite, surface d'attaque minimale | Pas de shell ; groupe `video` à ajouter au lancement (`group_add`) ; diagnostic plus difficile |
| `scratch` + binaire musl statique | Minimale | Cible musl + bindgen à valider ; aucun outil |

## Décision

- **Cross-compilation** dans une étape `builder` exécutée sur `$BUILDPLATFORM`, cible choisie
  d'après `$TARGETARCH` (`arm64` → `aarch64-unknown-linux-gnu` ; `amd64` → build natif, pour le
  développement et la CI uniquement ; toute autre plateforme est refusée),
  avec `gcc-aarch64-linux-gnu`, `clang`, `libclang-dev`, `linux-libc-dev-arm64-cross` et
  `BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_linux_gnu=--sysroot=/usr/aarch64-linux-gnu`
  (variable suffixée par la cible, lue par bindgen, pour ne pas affecter un build natif).
- Image finale **`debian:bookworm-slim`**, utilisateur `camstream` (UID 10001) membre du groupe
  `video`.
- **Pas de `curl` dans l'image** : le `HEALTHCHECK` appelle le binaire lui-même
  (`camstream healthcheck`), qui fait une requête HTTP minimale sur `127.0.0.1` avec la
  bibliothèque standard.
- Le processus Camstream est PID 1 et gère lui-même `SIGTERM`/`SIGINT` ; `init: true` est
  recommandé dans `docker-compose.yml` pour la récolte des processus zombies (sans effet
  fonctionnel ici, mais sans coût).

Distroless pourra remplacer `bookworm-slim` une fois le déploiement stabilisé (nouvel ADR).

## Conséquences

- Les détails (Dockerfile prévu, compose, variables) sont décrits dans
  [`../deploiement.md`](../deploiement.md).
- Le GID du groupe `video` doit correspondre entre l'hôte et le conteneur (44 sous Debian et
  Raspberry Pi OS) ; à défaut, utiliser `group_add` avec le GID numérique de `/dev/video0`.
- Le spike de cross-compilation de `v4l` (ADR 0003) conditionne la validation définitive de
  cet ADR. **Avancement (2026-11-26)** : cross-compilation **validée** avec les mêmes paquets et
  variables que le Dockerfile prévu, mais **hors Docker** (registre Docker Hub inaccessible
  depuis l'environnement du spike) ; le binaire ne requiert pas plus que `GLIBC_2.34`,
  compatible avec Bookworm. Le build `docker buildx` lui-même reste à exécuter
  ([spike 0001](../spikes/0001-v4l.md)). Statut inchangé : « Proposé ».
