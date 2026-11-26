# 0004 — Distribution des trames et modèle de concurrence

- Statut : Proposé
- Date : 2026-11-26

## Contexte

- La lecture V4L2 (`MmapStream::next`) est **bloquante** : elle ne doit jamais s'exécuter sur
  un thread du runtime tokio.
- Une trame capturée doit être livrée à **N clients** (0 à `CAMSTREAM_MAX_CLIENTS`).
- Un client lent (Wi-Fi faible, onglet en arrière-plan) **ne doit bloquer ni les autres
  clients ni la capture**, et ne doit pas faire croître la mémoire.
- Pour un flux vidéo en direct, une trame en retard n'a pas de valeur : mieux vaut la sauter.
- Mémoire limitée : une trame MJPEG 640x480 pèse ~30 à 80 Kio, une 1280x720 ~100 à 250 Kio.

## Options envisagées

### Exécution de la capture

| Option | Avantages | Inconvénients |
|---|---|---|
| **Thread OS dédié** (`std::thread::spawn`) | Durée de vie longue assumée ; aucun impact sur le pool bloquant de tokio ; arrêt explicite par jeton | Un thread à gérer (join à l'arrêt) |
| `tokio::task::spawn_blocking` | Intégré à tokio | Pool prévu pour des tâches finies ; une tâche infinie y occupe un slot et retarde l'arrêt du runtime |
| Lecture non bloquante + `AsyncFd` | 100 % async | Non proposé par `v4l` ; code `unsafe`/bas niveau |

### Diffusion vers les clients

| Option | Avantages | Inconvénients |
|---|---|---|
| `Arc<Mutex<Option<Frame>>>` + `Notify` | Simple | État partagé sous verrou (déconseillé par `CLAUDE.md`) ; logique de notification maison |
| `tokio::sync::watch` | Conserve uniquement la dernière trame : un client lent saute naturellement les trames ; mémoire constante | Pas de file : impossible de transporter des événements ordonnés ; `borrow()` garde un verrou de lecture pendant la lecture (à cloner immédiatement) |
| **`tokio::sync::broadcast`** | Pub/Sub natif ; `send` **synchrone et non bloquant** (appelable depuis le thread de capture) ; tampon circulaire borné ; un récepteur en retard reçoit `RecvError::Lagged(n)` au lieu de bloquer l'émetteur ; `Closed` signale la fin du flux | Le tampon conserve `capacity` trames (mémoire bornée mais non nulle) ; après `Lagged`, le récepteur repart de la plus ancienne trame encore en tampon |
| `mpsc` par client + `try_send` | Contrôle fin par client | Fan-out et registre des clients à écrire à la main |

## Décision

1. **Capture dans un thread OS dédié** (`camstream-capture`) exécutant le
   `CaptureSupervisor` : ouverture de la source via la fabrique, boucle de lecture,
   reconnexion avec attente exponentielle, arrêt sur `CancellationToken`.
2. **Diffusion par `tokio::sync::broadcast`** encapsulée dans `FrameHub` (module `stream`) :
   - capacité **petite et configurable** (`CAMSTREAM_HUB_CAPACITY`, défaut **4**) : elle absorbe
     la gigue réseau sans accumuler de latence (4 trames à 15 i/s ≈ 270 ms au maximum) ;
     `broadcast` arrondit la capacité à la puissance de deux supérieure (5 → 8) ;
   - `Frame` contient un `Bytes` : le clonage par récepteur ne copie pas l'image ;
   - politique **clients lents** : sur `Lagged(n)`, le `FrameSubscriber` journalise en `debug`,
     incrémente un compteur de trames sautées et **continue** avec la trame suivante
     disponible ; il ne coupe pas le client ;
   - `publish` avec 0 abonné n'est pas une erreur (la capture continue, la trame est jetée).
3. **État de la source par `tokio::sync::watch<SourceStatus>`** (`Starting`, `Streaming`,
   `Reconnecting`, `Failed`, `Stopped`), lu par `/health` et par les logs.
4. **Arrêt coordonné par `tokio_util::sync::CancellationToken`** : un seul jeton racine,
   des enfants pour le serveur et la capture.

`watch` reste une alternative valable pour les trames (sémantique « dernière image ») : si les
mesures sur Pi montrent une latence ou une mémoire excessive avec `broadcast`, le remplacement
est local à `FrameHub` (l'API publique `publish` / `subscribe` / `next_frame` ne change pas).

## Conséquences

- Mémoire de diffusion bornée : `capacité × taille de trame` (≈ 1 Mio au pire en 720p), quel
  que soit le nombre de clients.
- Le thread de capture n'attend jamais un client ; la seule copie par trame est celle du tampon
  mmap vers un `Bytes` (inévitable car le tampon mmap est rendu au pilote).
- Le superviseur doit vérifier le jeton d'annulation entre deux lectures ; la lecture V4L2 doit
  avoir un **délai maximal** (poll avec timeout, ex. 2 s) pour qu'une webcam figée n'empêche pas
  l'arrêt.
- Le comportement « client lent » est testable sans réseau : abonné qui ne lit pas, émission de
  `capacité + k` trames, vérification de `Lagged` puis reprise.
