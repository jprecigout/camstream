# Camstream

Camstream capture le flux d'une **webcam USB** (V4L2) et le diffuse dans un **navigateur web**,
sans ré-encodage : les images JPEG produites par la webcam (MJPEG) sont relayées telles quelles
en `multipart/x-mixed-replace`, lisible nativement par une simple balise `<img>`.

Conçu pour un **Raspberry Pi** (Raspberry Pi OS 64 bits, `aarch64`) dans **Docker**.

- **Capture à la demande** : la webcam ne s'allume qu'au premier spectateur et s'éteint
  10 s après le départ du dernier.
- **Plusieurs spectateurs** (10 par défaut) ; un client lent saute des images sans ralentir
  les autres.
- **Reconnexion automatique** de la webcam débranchée puis rebranchée.
- **Mire de test** intégrée pour essayer sans webcam (`--source synthetic`).
- Endpoint de santé `/health` et sous-commande `camstream healthcheck` pour Docker.

> **Sécurité** : la v1 n'a **ni authentification ni TLS**. Elle est réservée au **réseau
> local** : ne jamais l'exposer sur Internet (voir [ADR 0007](docs/adr/0007-securite-reseau-local-v1.md)
> et [déploiement § 9](docs/deploiement.md#9-sécurité)).

## Démarrage rapide

### Sans webcam (mire de test), sur un poste de développement Linux

Prérequis : Rust stable (1.88 ou plus), `clang` et `libclang-dev` (bindings V4L2 générés au build).

```bash
cargo run -- --source synthetic
# puis ouvrir http://127.0.0.1:8080/
```

Sans `clang`, compiler sans l'adaptateur V4L2 (seule la mire est alors disponible) :

```bash
cargo run --no-default-features -- --source synthetic
```

### Avec une webcam

```bash
cargo run -- --device /dev/video0 --bind 0.0.0.0 --port 8080
```

La webcam doit proposer le format **MJPEG** (`v4l2-ctl -d /dev/video0 --list-formats-ext`) et
l'utilisateur doit appartenir au groupe `video`. Par défaut, Camstream n'écoute que sur
`127.0.0.1` : `--bind 0.0.0.0` (ou l'adresse LAN) le rend joignable depuis le réseau local.

### Avec Docker sur le Raspberry Pi

L'image est construite sur un PC x86_64 par compilation croisée (voir
[deploiement.md § 3](docs/deploiement.md#3-dockerfile-multi-étapes-cross-compilation)) :

```bash
docker buildx build --platform linux/arm64 -t camstream:latest --load .
docker save camstream:latest | gzip | ssh pi@raspberrypi.local 'gunzip | docker load'
```

Puis sur le Pi, en adaptant l'adresse LAN :

```bash
docker run -d --name camstream \
    --device /dev/video0:/dev/video0 --group-add video \
    -p 192.168.1.20:8080:8080 \
    --read-only --cap-drop ALL --security-opt no-new-privileges:true \
    --restart unless-stopped \
    camstream:latest
```

Un [`docker-compose.yml`](docker-compose.yml) d'exemple est fourni.

## Points d'accès

| Route | Rôle |
|---|---|
| `GET /` | Page HTML affichant le flux (« Démarrage de la caméra… » jusqu'à la première image) |
| `GET /stream.mjpg` | Flux MJPEG `multipart/x-mixed-replace` ; `503` au-delà de la limite de clients |
| `GET /health` | État JSON, ex. `{"status":"ok","camera":{"state":"idle"},"clients":0}` ; `503` si la capture a échoué |

## Configuration

Toutes les options existent en ligne de commande et en variable d'environnement
(`CAMSTREAM_*`) ; `camstream --help` les liste. Principales :

| Variable | Défaut | Rôle |
|---|---|---|
| `CAMSTREAM_SOURCE` | `v4l2` | `v4l2` ou `synthetic` (mire de test) |
| `CAMSTREAM_DEVICE` | `/dev/video0` | Périphérique V4L2 |
| `CAMSTREAM_RESOLUTION` / `CAMSTREAM_FPS` | `640x480` / `15` | Paramètres demandés à la webcam |
| `CAMSTREAM_BIND` / `CAMSTREAM_PORT` | `127.0.0.1` / `8080` | Écoute HTTP (`0.0.0.0` dans l'image Docker) |
| `CAMSTREAM_IDLE_TIMEOUT_MS` | `10000` | Délai avant d'éteindre la webcam sans spectateur |
| `CAMSTREAM_MAX_CLIENTS` | `10` | Spectateurs simultanés maximum |

Liste complète : [deploiement.md § 5](docs/deploiement.md#5-variables-denvironnement).
Codes de sortie : `0` arrêt normal, `1` échec (capture, serveur, healthcheck), `2` configuration
invalide.

## Développement

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --no-default-features   # sans l'adaptateur V4L2
```

Les tests n'ont besoin d'aucune webcam (mire synthétique et sources scriptées). Les règles du
projet (langues, commentaires, Conventional Commits, revue avant commit) sont dans
[`CLAUDE.md`](CLAUDE.md).

## Documentation

| Document | Contenu |
|---|---|
| [docs/architecture.md](docs/architecture.md) | Modules, ports et adaptateurs, flux de données, erreurs, cycle de vie de la capture |
| [docs/design-patterns.md](docs/design-patterns.md) | Design patterns utilisés et emplacements dans le code |
| [docs/deploiement.md](docs/deploiement.md) | Docker, Raspberry Pi, accès à la webcam, variables d'environnement, sécurité |
| [docs/adr/](docs/adr/) | Décisions d'architecture |
| [docs/spikes/0001-v4l.md](docs/spikes/0001-v4l.md) | Spike sur la crate `v4l` (mesures sur Pi à venir) |
