# Déploiement : Docker et Raspberry Pi

> Statut : **conception initiale (proposée)**. Le `Dockerfile` et le `docker-compose.yml`
> ci-dessous sont les versions **prévues** ; ils seront créés à la racine du dépôt lors de
> l'implémentation, et ce document mis à jour dans le même commit. Décisions :
> [ADR 0005](adr/0005-image-docker-et-compilation-arm64.md).

## 1. Cibles

| Matériel | OS | Plateforme Docker | Cible Rust | Statut |
|---|---|---|---|---|
| Raspberry Pi 3 / 4 / 5, Zero 2 W | Raspberry Pi OS 64 bits (Bookworm) | `linux/arm64` | `aarch64-unknown-linux-gnu` | Cible principale |
| Raspberry Pi 2 / 3 / Zero 2 W | Raspberry Pi OS 32 bits | `linux/arm/v7` | `armv7-unknown-linux-gnueabihf` | Optionnelle |
| Poste de développement | Linux x86_64 | `linux/amd64` | `x86_64-unknown-linux-gnu` | Développement, CI |

Les Raspberry Pi 1 et Zero (première génération, ARMv6) ne sont pas visés.

## 2. Diagramme de déploiement

```mermaid
flowchart LR
    subgraph LAN["Réseau local"]
        Browser["Navigateur<br/>PC, téléphone"]
    end

    subgraph Pi["Raspberry Pi - Raspberry Pi OS 64 bits"]
        USB["Webcam USB"] --> Node["/dev/video0<br/>groupe video, GID 44"]
        subgraph Engine["Docker Engine"]
            subgraph C["Conteneur camstream<br/>utilisateur camstream UID 10001, groupe video"]
                Bin["/usr/local/bin/camstream<br/>port 8080"]
                HC["HEALTHCHECK<br/>camstream healthcheck"]
            end
        end
        Node -->|"--device /dev/video0"| Bin
        HC -->|"GET 127.0.0.1:8080/health"| Bin
    end

    subgraph Build["Machine de build x86_64"]
        Buildx["docker buildx<br/>cross-compilation arm64"]
    end

    Registry[("Registre d'images<br/>GHCR ou local")]
    Buildx -->|"push linux/arm64"| Registry
    Registry -->|"pull"| Engine
    Browser -->|"HTTP 8080<br/>/ et /stream.mjpg"| Bin
```

## 3. Dockerfile prévu (multi-étapes, cross-compilation)

Principe : l'étape `builder` s'exécute sur l'architecture de la machine de build
(`$BUILDPLATFORM`) et produit un binaire pour `$TARGETPLATFORM` ; l'image finale ne contient
que le binaire et la glibc.

```dockerfile
# syntax=docker/dockerfile:1.7

# --- Étape 1 : compilation croisée sur la plateforme de build (rapide, sans QEMU) ---
FROM --platform=$BUILDPLATFORM rust:1-bookworm AS builder
ARG TARGETARCH

# Outils croisés + clang/libclang pour bindgen (crate v4l) + en-têtes noyau de la cible.
RUN apt-get update && apt-get install -y --no-install-recommends \
        clang libclang-dev \
        gcc-aarch64-linux-gnu libc6-dev-arm64-cross linux-libc-dev-arm64-cross \
        gcc-arm-linux-gnueabihf libc6-dev-armhf-cross linux-libc-dev-armhf-cross \
    && rm -rf /var/lib/apt/lists/*

# Correspondance plateforme Docker -> cible Rust et sysroot pour bindgen.
RUN case "$TARGETARCH" in \
      arm64) echo "aarch64-unknown-linux-gnu aarch64-linux-gnu" ;; \
      arm)   echo "armv7-unknown-linux-gnueabihf arm-linux-gnueabihf" ;; \
      amd64) echo "x86_64-unknown-linux-gnu x86_64-linux-gnu" ;; \
      *) echo "unsupported arch $TARGETARCH" >&2; exit 1 ;; \
    esac > /target.txt \
    && rustup target add "$(cut -d' ' -f1 /target.txt)"

# Éditeurs de liens croisés, et sysroot bindgen **par cible** : bindgen lit
# BINDGEN_EXTRA_CLANG_ARGS_<cible> (tirets remplacés par « _ ») d'après la variable TARGET
# du script de build, donc un build natif amd64 n'est pas affecté (il n'existe pas de
# /usr/x86_64-linux-gnu sur un hôte amd64 : une sysroot globale y casserait bindgen).
# clang cherche les en-têtes dans <sysroot>/include, où les paquets *-cross les installent.
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER=arm-linux-gnueabihf-gcc \
    BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_linux_gnu=--sysroot=/usr/aarch64-linux-gnu \
    BINDGEN_EXTRA_CLANG_ARGS_armv7_unknown_linux_gnueabihf=--sysroot=/usr/arm-linux-gnueabihf

WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,sharing=locked \
    set -eu; \
    RUST_TARGET="$(cut -d' ' -f1 /target.txt)"; \
    cargo build --release --locked --target "$RUST_TARGET"; \
    install -D "target/${RUST_TARGET}/release/camstream" /out/camstream

# --- Étape 2 : image d'exécution minimale ---
FROM debian:bookworm-slim

# Utilisateur non root, membre du groupe video (GID 44 sous Debian).
RUN useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin \
        --groups video camstream

COPY --from=builder /out/camstream /usr/local/bin/camstream

# Forme « nom seul » volontaire : avec « USER camstream:camstream » (groupe explicite),
# le runtime n'applique pas les groupes supplémentaires de /etc/group et le processus
# perdrait l'appartenance au groupe video.
USER camstream
ENV CAMSTREAM_BIND=0.0.0.0 \
    CAMSTREAM_PORT=8080 \
    RUST_LOG=info
EXPOSE 8080
STOPSIGNAL SIGTERM

# Sonde sans curl : le binaire interroge lui-même /health sur 127.0.0.1.
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/camstream", "healthcheck"]

ENTRYPOINT ["/usr/local/bin/camstream"]
```

Remarques :

- l'installation des deux chaînes croisées est volontairement simple ; elle pourra être
  conditionnée à `$TARGETARCH` si la taille de l'étape de build devient gênante (elle
  n'affecte pas l'image finale) ;
- le build natif `x86_64` n'utilise aucune sysroot bindgen (variables suffixées par cible) ;
  en revanche, une machine de build **arm64** (Mac Apple Silicon, runner ARM) ne dispose pas
  des paquets `gcc-aarch64-linux-gnu`/`*-arm64-cross` : l'installation des chaînes croisées
  devra alors être conditionnée à `$BUILDARCH` ≠ `$TARGETARCH` ;
- le cache `target` est en `sharing=locked` : les builds `arm64` et `arm/v7` lancés en
  parallèle par `buildx` s'y succèdent au lieu de s'y écrire simultanément ;
- la ligne `--locked` impose un `Cargo.lock` versionné (application, donc normal).

Commandes :

```bash
# Image arm64 chargée localement (pour test) :
docker buildx build --platform linux/arm64 -t camstream:latest --load .

# Image multi-architecture poussée vers un registre :
docker buildx build --platform linux/arm64,linux/arm/v7 \
    -t ghcr.io/jprecigout/camstream:latest --push .
```

## 4. Exécution

### Commande `docker run`

```bash
docker run -d --name camstream \
    --device /dev/video0:/dev/video0 \
    --group-add video \
    -p 8080:8080 \
    --read-only --cap-drop ALL --security-opt no-new-privileges:true \
    --restart unless-stopped \
    camstream:latest
```

Jamais `--privileged` : seul le nœud `/dev/video0` est exposé.

### `docker-compose.yml` d'exemple

```yaml
services:
  camstream:
    image: ghcr.io/jprecigout/camstream:latest
    # build: .                       # alternative : construire localement
    restart: unless-stopped
    init: true                       # récolte des zombies ; SIGTERM transmis à camstream
    devices:
      - "/dev/video0:/dev/video0"
    group_add:
      - "video"                      # ou le GID numérique : stat -c %g /dev/video0
    ports:
      - "8080:8080"
    environment:
      CAMSTREAM_DEVICE: /dev/video0
      CAMSTREAM_RESOLUTION: 640x480
      CAMSTREAM_FPS: "15"
      CAMSTREAM_MAX_CLIENTS: "10"
      RUST_LOG: info
    read_only: true
    cap_drop: [ALL]
    security_opt:
      - "no-new-privileges:true"
    stop_grace_period: 10s           # supérieur à CAMSTREAM_SHUTDOWN_TIMEOUT_MS
    mem_limit: 128m
    cpus: 1.0
    logging:
      driver: json-file
      options:
        max-size: "10m"              # préserver la carte SD
        max-file: "3"
```

## 5. Variables d'environnement

Chaque variable a son option CLI équivalente (clap, feature `env`) ; la CLI a priorité sur
l'environnement. Les valeurs invalides provoquent un arrêt immédiat avec un message explicite.

| Variable | Option CLI | Défaut | Description |
|---|---|---|---|
| `CAMSTREAM_SOURCE` | `--source` | `v4l2` | Type de source : `v4l2` ou `synthetic` (mire de test, sans webcam) |
| `CAMSTREAM_DEVICE` | `--device` | `/dev/video0` | Périphérique V4L2 |
| `CAMSTREAM_RESOLUTION` | `--resolution` | `640x480` | Résolution demandée (`LARGEURxHAUTEUR`) |
| `CAMSTREAM_FPS` | `--fps` | `15` | Cadence demandée |
| `CAMSTREAM_FORMAT` | `--format` | `auto` | `auto` (MJPEG puis YUYV), `mjpeg`, `yuyv` |
| `CAMSTREAM_READ_TIMEOUT_MS` | `--read-timeout-ms` | `2000` | Délai maximal d'attente d'une trame |
| `CAMSTREAM_BIND` | `--bind` | `0.0.0.0` | Adresse d'écoute HTTP |
| `CAMSTREAM_PORT` | `--port` | `8080` | Port HTTP (utilisé aussi par `camstream healthcheck`) |
| `CAMSTREAM_MAX_CLIENTS` | `--max-clients` | `10` | Clients de flux simultanés maximum (au-delà : `503`) |
| `CAMSTREAM_HUB_CAPACITY` | `--hub-capacity` | `4` | Trames conservées dans le tampon de diffusion (arrondi à la puissance de deux supérieure) |
| `CAMSTREAM_RECONNECT_INITIAL_MS` | `--reconnect-initial-ms` | `1000` | Première attente avant reconnexion de la webcam |
| `CAMSTREAM_RECONNECT_MAX_MS` | `--reconnect-max-ms` | `30000` | Attente maximale entre deux tentatives |
| `CAMSTREAM_SHUTDOWN_TIMEOUT_MS` | `--shutdown-timeout-ms` | `5000` | Délai maximal de l'arrêt gracieux |
| `CAMSTREAM_JPEG_QUALITY` | `--jpeg-quality` | `80` | Qualité JPEG du repli YUYV (phase 2) |
| `RUST_LOG` | — | `info` | Filtre `tracing` (ex. `camstream=debug`) |

Ordre de grandeur réseau : en 640x480 à 15 i/s, un flux MJPEG représente environ 0,5 à
1 Mo/s (4 à 8 Mbit/s) **par client**. Dix clients saturent facilement le Wi-Fi d'un Pi :
ajuster `CAMSTREAM_MAX_CLIENTS`, la résolution et la cadence en conséquence.

## 6. Accès à la webcam et droits

1. Vérifier la webcam sur l'hôte : `v4l2-ctl --list-devices` puis
   `v4l2-ctl -d /dev/video0 --list-formats-ext` (présence de `MJPG`).
2. Vérifier le groupe du nœud : `stat -c '%G %g' /dev/video0` (attendu : `video 44`).
3. Si le GID diffère de celui du groupe `video` de l'image, utiliser le GID numérique dans
   `group_add`.
4. Symptôme d'un problème de droits : statut `failed` et log `permission denied on video
   device` → erreur fatale volontaire (voir [architecture](architecture.md#6-gestion-des-erreurs)).

Une webcam USB expose souvent **deux** nœuds (`/dev/video0` capture, `/dev/video1`
métadonnées) : seul le nœud de capture est à exposer.

## 7. Reconnexion de la webcam dans Docker

L'application gère la perte et le retour de la webcam (statut `reconnecting`, attente
exponentielle). Mais avec `--device`, Docker crée le nœud dans le conteneur **au démarrage**,
avec les numéros majeur/mineur de ce moment-là :

- si la webcam est rebranchée et retrouve le **même** nœud (`/dev/video0`, 81:0), la
  reconnexion fonctionne sans redémarrer le conteneur ;
- si elle obtient un autre mineur (ex. `/dev/video2`, autre périphérique branché entre-temps),
  le nœud du conteneur ne correspond plus ;
- si `/dev/video0` n'existe pas au démarrage, Docker refuse de lancer le conteneur.

Options (à valider, voir points ouverts) :

| Option | Avantages | Inconvénients |
|---|---|---|
| `--device` simple (défaut) | Le plus restrictif | Reconnexion limitée au même nœud |
| `device_cgroup_rules: ["c 81:* rmw"]` + montage de `/dev/v4l/by-id` (lien stable) | Reconnexion robuste, chemin stable par webcam | Autorise tous les périphériques V4L2 de l'hôte ; montage de `/dev` partiel |
| Règle udev sur l'hôte créant un lien fixe (`/dev/camstream0`) | Chemin stable | Configuration hôte supplémentaire ; même limite de nœud au démarrage |

## 8. Santé et arrêt

- `GET /health` : `200` tant que l'application tourne (y compris pendant une reconnexion de
  la webcam, l'état détaillé est dans le corps JSON) ; `503` si l'état est `failed`.
- `camstream healthcheck` : sous-commande qui interroge `http://127.0.0.1:$CAMSTREAM_PORT/health`
  avec la bibliothèque standard (pas de `curl` dans l'image), code de sortie 0 ou 1.
- `docker stop` envoie `SIGTERM` : l'application arrête d'accepter des connexions, termine les
  flux en cours, arrête la capture et libère la webcam en moins de
  `CAMSTREAM_SHUTDOWN_TIMEOUT_MS` ; `stop_grace_period` doit être supérieur.
- Docker ne redémarre **pas** un conteneur seulement `unhealthy` (hors Swarm) : l'état de santé
  sert au diagnostic et à la supervision externe ; le redémarrage automatique repose sur la
  sortie du processus (erreur fatale → code 1 → `restart: unless-stopped`).

## 9. Sécurité

- Utilisateur non root, `--cap-drop ALL`, `no-new-privileges`, système de fichiers en lecture
  seule (l'application n'écrit rien sur disque).
- Pas d'authentification ni de TLS dans Camstream en v1 : à exposer uniquement sur le réseau
  local, ou derrière un reverse proxy assurant TLS et authentification.
