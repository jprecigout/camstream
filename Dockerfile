# syntax=docker/dockerfile:1.7
# Image Camstream : compilation croisée vers arm64 (Raspberry Pi OS 64 bits), image
# finale minimale et non root. Décisions : docs/adr/0005, détails : docs/deploiement.md.

# --- Étape 1 : compilation sur la plateforme de build (rapide, sans QEMU) ---
FROM --platform=$BUILDPLATFORM rust:1-bookworm AS builder
ARG BUILDARCH
ARG TARGETARCH

# clang/libclang pour bindgen (crate v4l). Chaîne croisée arm64 et en-têtes noyau arm64
# uniquement si la machine de build n'est pas elle-même arm64.
RUN set -eu; \
    packages="clang libclang-dev"; \
    if [ "$BUILDARCH" != "$TARGETARCH" ]; then \
      case "$TARGETARCH" in \
        arm64) packages="$packages gcc-aarch64-linux-gnu libc6-dev-arm64-cross linux-libc-dev-arm64-cross" ;; \
        *) echo "unsupported cross target $TARGETARCH (arm64 only)" >&2; exit 1 ;; \
      esac; \
    fi; \
    apt-get update; \
    apt-get install -y --no-install-recommends $packages; \
    rm -rf /var/lib/apt/lists/*

# Correspondance plateforme Docker -> cible Rust (arm64 en production, amd64 pour le dev).
RUN case "$TARGETARCH" in \
      arm64) echo "aarch64-unknown-linux-gnu" ;; \
      amd64) echo "x86_64-unknown-linux-gnu" ;; \
      *) echo "unsupported arch $TARGETARCH (arm64 only)" >&2; exit 1 ;; \
    esac > /target.txt \
    && rustup target add "$(cat /target.txt)"

WORKDIR /src
COPY . .
# Éditeur de liens croisé et sysroot bindgen **par cible** (variables suffixées), et
# seulement en compilation croisée : un build natif n'est pas affecté.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,sharing=locked \
    set -eu; \
    RUST_TARGET="$(cat /target.txt)"; \
    if [ "$BUILDARCH" != "$TARGETARCH" ]; then \
      export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc; \
      export BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_linux_gnu=--sysroot=/usr/aarch64-linux-gnu; \
    fi; \
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

# Sonde sans curl : le binaire interroge lui-même /health sur 127.0.0.1. Sa durée
# totale est bornée par CAMSTREAM_HEALTHCHECK_TIMEOUT_MS (2 s par défaut), inférieure
# au --timeout de 3 s : la sonde conclut toujours (code 0 ou 1) avant que Docker la tue.
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/camstream", "healthcheck"]

ENTRYPOINT ["/usr/local/bin/camstream"]
