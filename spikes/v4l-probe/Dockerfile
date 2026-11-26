# syntax=docker/dockerfile:1.7
#
# Image du spike v4l-probe. Reprend à l'identique la cross-compilation prévue dans
# docs/deploiement.md (étape de build sur $BUILDPLATFORM, cible d'après $TARGETARCH,
# sysroot bindgen suffixée par la cible) afin de valider cette approche.
# Contexte de build : ce répertoire (spikes/v4l-probe).
#
#   docker buildx build --platform linux/arm64 -t v4l-probe:spike --load .

# --- Étape 1 : compilation croisée sur la plateforme de build (sans QEMU) ---
FROM --platform=$BUILDPLATFORM rust:1-bookworm AS builder
ARG TARGETARCH

# Outils croisés arm64 + clang/libclang pour bindgen (crate v4l) + en-têtes noyau arm64.
RUN apt-get update && apt-get install -y --no-install-recommends \
        clang libclang-dev \
        gcc-aarch64-linux-gnu libc6-dev-arm64-cross linux-libc-dev-arm64-cross \
    && rm -rf /var/lib/apt/lists/*

RUN case "$TARGETARCH" in \
      arm64) echo "aarch64-unknown-linux-gnu" ;; \
      amd64) echo "x86_64-unknown-linux-gnu" ;; \
      *) echo "unsupported arch $TARGETARCH (arm64 only)" >&2; exit 1 ;; \
    esac > /target.txt \
    && rustup target add "$(cat /target.txt)"

# La sysroot empêche clang de compléter les en-têtes arm64 par ceux de l'hôte
# (/usr/include), qu'il ajoute sinon à la fin du chemin de recherche.
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_linux_gnu=--sysroot=/usr/aarch64-linux-gnu

WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,sharing=locked \
    set -eu; \
    RUST_TARGET="$(cat /target.txt)"; \
    cargo build --release --locked --target "$RUST_TARGET"; \
    install -D "target/${RUST_TARGET}/release/v4l-probe" /out/v4l-probe

# --- Étape 2 : image d'exécution minimale ---
FROM debian:bookworm-slim

# Utilisateur non root ; l'accès à la webcam passe par --group-add video au lancement.
RUN useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin \
        --groups video probe

COPY --from=builder /out/v4l-probe /usr/local/bin/v4l-probe

# Forme « nom seul » : conserve les groupes supplémentaires (video) de /etc/group.
USER probe
WORKDIR /tmp
# Outil ponctuel (une exécution, un rapport) : pas de HEALTHCHECK.
ENTRYPOINT ["/usr/local/bin/v4l-probe"]
