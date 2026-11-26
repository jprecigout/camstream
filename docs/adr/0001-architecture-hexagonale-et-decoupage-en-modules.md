# 0001 — Architecture hexagonale et découpage en modules

- Statut : Proposé
- Date : 2026-11-26

## Contexte

Camstream capture le flux d'une webcam USB (V4L2) et le diffuse dans un navigateur, sur un
Raspberry Pi, dans Docker. Les exigences structurantes sont :

- le cœur (distribution des trames) ne doit dépendre **ni de V4L2 ni du framework HTTP** ;
- toute dépendance matérielle doit être remplaçable par une implémentation de test
  (CI sans webcam, tests unitaires déterministes) ;
- le transport doit pouvoir évoluer (MJPEG aujourd'hui, WebSocket ou WebRTC demain) et la
  source aussi (plusieurs caméras, fichier, mire de test) ;
- le projet est petit : la structure doit rester simple à lire (YAGNI).

## Options envisagées

### Option A — Application monolithique « en couches implicites »

Un `main.rs` qui ouvre la webcam, lance axum et partage la dernière trame via un
`Arc<Mutex<Vec<u8>>>`.

- Avantages : très peu de code, démarrage rapide.
- Inconvénients : couplage fort V4L2 ↔ HTTP, impossible à tester sans webcam, verrou partagé
  entre capture et clients (un client lent ou un verrou tenu trop longtemps ralentit la capture),
  aucune place pour la reconnexion ou un second transport.

### Option B — Architecture hexagonale dans une seule crate, découpée en modules

Un cœur (`domain` + `stream`) définit les types et les **ports** (traits) ; `capture` et
`server` sont des **adaptateurs** ; `config` est isolé ; `app` est la racine de composition.

- Avantages : inversion de dépendances respectée, testabilité par substitution des ports,
  évolutivité par ajout d'adaptateurs, une seule crate donc un seul `Cargo.toml` et un build
  Docker simple.
- Inconvénients : la frontière entre modules n'est garantie que par discipline et revue (pas par
  le compilateur au niveau crate).

### Option C — Workspace Cargo multi-crates (`camstream-core`, `camstream-v4l2`, `camstream-http`…)

- Avantages : frontières vérifiées par le compilateur, compilation conditionnelle naturelle.
- Inconvénients : surcoût de maintenance (versions, features, CI) disproportionné pour la taille
  du projet ; build Docker plus complexe.

## Décision

**Option B** : architecture hexagonale (ports et adaptateurs) dans une crate unique
`camstream` (binaire + bibliothèque `lib.rs` pour les tests d'intégration), avec les modules :

| Module | Rôle | Dépend de |
|---|---|---|
| `domain` | Types métier (`Frame`, `Resolution`, `Fps`…), ports (`FrameSource`, `FrameSourceFactory`), erreurs de capture | `bytes`, `thiserror` uniquement |
| `stream` | Cœur : diffuseur de trames (`FrameHub`), superviseur de capture (`CaptureSupervisor`), état de la source | `domain`, `tokio::sync` |
| `capture` | Adaptateurs de source : V4L2, source synthétique, fabrique | `domain`, crate `v4l` |
| `server` | Adaptateur HTTP : page HTML, flux MJPEG, `/health` | `stream`, `domain`, `axum` |
| `config` | Lecture et validation de la configuration (CLI + variables d'environnement) | `domain`, `clap` |
| `app` + `main.rs` | Racine de composition : câblage, signaux, arrêt propre | tous, `anyhow` |

Règles de dépendance (vérifiées en revue d'architecture) :

- `domain` et `stream` n'importent jamais `v4l`, `axum`, `hyper` ni `clap` ;
- `capture` et `server` ne se connaissent pas ;
- seul `app`/`main.rs` connaît tous les modules et utilise `anyhow`.

Si une frontière est violée de façon répétée, ou si une seconde caméra impose des
compilations conditionnelles lourdes, la migration vers l'option C sera réévaluée par un
nouvel ADR.

## Conséquences

- Les tests du cœur et du serveur utilisent la source synthétique (`SyntheticSource`) et une
  fabrique de test : aucun besoin de webcam en CI.
- L'adaptateur V4L2 est placé derrière une feature Cargo `v4l2` (activée par défaut) afin que
  le cœur reste compilable sans en-têtes V4L2.
- Ajouter WebSocket/WebRTC revient à ajouter un adaptateur dans `server` (ou un module
  `transport` dédié) qui s'abonne au `FrameHub` ; le cœur ne change pas.
- Plusieurs caméras : un `CaptureSupervisor` + un `FrameHub` par caméra, sans changement des
  ports.
- La conception détaillée est décrite dans [`../architecture.md`](../architecture.md).
