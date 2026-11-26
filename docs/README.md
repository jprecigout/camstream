# Documentation Camstream

Toute la documentation du projet est rédigée en français et les diagrammes sont faits en Mermaid.

| Document | Contenu |
|---|---|
| [architecture.md](architecture.md) | Vue d'ensemble, modules, ports et adaptateurs, flux de données, erreurs, concurrence, cycle de vie |
| [design-patterns.md](design-patterns.md) | Catalogue des design patterns utilisés (et écartés) |
| [deploiement.md](deploiement.md) | Docker, Raspberry Pi, accès à la webcam, variables d'environnement |
| [adr/](adr/) | Décisions d'architecture (ADR) |

## Décisions d'architecture (ADR)

| N° | Titre | Statut |
|---|---|---|
| [0001](adr/0001-architecture-hexagonale-et-decoupage-en-modules.md) | Architecture hexagonale et découpage en modules | Proposé |
| [0002](adr/0002-transport-mjpeg-multipart.md) | Transport vidéo : MJPEG en `multipart/x-mixed-replace` | Proposé |
| [0003](adr/0003-choix-des-crates.md) | Choix des crates (runtime, HTTP, V4L2, erreurs, configuration, logs) | Proposé |
| [0004](adr/0004-distribution-des-trames-et-concurrence.md) | Distribution des trames et modèle de concurrence | Proposé |
| [0005](adr/0005-image-docker-et-compilation-arm64.md) | Image Docker et compilation pour arm64 (cible unique) | Proposé |
| [0006](adr/0006-capture-a-la-demande.md) | Capture à la demande | Proposé |
| [0007](adr/0007-securite-reseau-local-v1.md) | Sécurité de la v1 : réseau local, sans authentification ni TLS | Proposé |

## Spikes

Expérimentations techniques jetables qui lèvent une incertitude avant implémentation ; le code
correspondant est dans [`../spikes/`](../spikes/), isolé de l'application.

| N° | Sujet | Statut |
|---|---|---|
| [0001](spikes/0001-v4l.md) | Crate `v4l` : cross-compilation arm64 et comportement réel | En cours (cross-compilation validée, mesures Pi à faire) |

Les règles du projet sont décrites dans [`CLAUDE.md`](../CLAUDE.md).
