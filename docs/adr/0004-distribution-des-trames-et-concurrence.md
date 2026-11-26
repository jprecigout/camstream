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
3. **État de la source par `tokio::sync::watch<SourceStatus>`** (`Starting`, `Idle`,
   `Streaming`, `Reconnecting`, `Failed`, `Stopped`), lu par `/health` et par les logs.
   L'état `Idle` et le signal de demande (nombre d'abonnés) sont introduits par
   l'[ADR 0006](0006-capture-a-la-demande.md).
4. **Arrêt coordonné par `tokio_util::sync::CancellationToken`** : un seul jeton racine,
   des enfants pour le serveur et la capture.
5. **Erreur fatale de capture remontée par l'état, pas par le jeton.** Le superviseur reçoit
   un jeton *enfant* (annuler un enfant n'annule pas le parent) et ne connaît pas le cycle de
   vie global : sur erreur `Fatal`, il publie `Failed` et termine son thread. `main` attend,
   dans son `select!` principal, `SIGTERM`/`SIGINT`, la fin du serveur **ou**
   `status.wait_for(Failed)` ; dans ce dernier cas il annule le jeton racine et sort en code 1.
   Écarté : donner le jeton racine au superviseur (il pourrait arrêter toute l'application,
   couplage du cœur au cycle de vie applicatif) ; canal `oneshot` dédié (doublon de `watch`).
   **Complément (implémentation, 2026-11-26)** : une **fin du thread de capture sans `Failed`**
   (panique) est une fin anormale, traitée comme une erreur fatale (code 1) : un serveur sans
   capture vivante ne diffuserait plus rien tout en se déclarant sain. Un gardien RAII
   (`PanicGuard`) publie `Failed` pendant le déroulement de la panique ; `app` traite en plus
   la fermeture du canal d'état sans `Failed` comme une fin anormale, et
   `CaptureHandle::join` renvoie `CaptureThreadError::Panicked`. Les paniques des `Drop` de
   `v4l` sont contenues dans l'adaptateur (`catch_unwind`). Voir
   [architecture § 7.1](../architecture.md#71-décisions-prises-à-limplémentation-points-ouverts-de-la-revue).
6. **Limite de clients par `tokio::sync::Semaphore`** (`MaxClients` permis) dans `server` :
   `Arc<Semaphore>::try_acquire_owned()` réserve une place sans attente (échec → `503`) et
   renvoie un `OwnedSemaphorePermit` libéré à la destruction (**RAII**). Remplace le compteur
   `AtomicUsize` + `compare_exchange` de la première version : même coût, aucune course
   possible par construction, aucun code de synchronisation maison. Le nombre de clients
   exposé par `/health` vaut `max - available_permits()`.
7. **Clients bloqués en écriture TCP.** Un client lent mais vivant ne bloque personne (il
   saute des trames) ; un client **disparu sans `RST`** (Wi-Fi coupé, téléphone en veille)
   bloquerait en revanche son écriture indéfiniment, garderait sa place et, avec la capture à
   la demande, maintiendrait la webcam allumée. Décision : sur chaque socket acceptée (via
   `axum::serve::ListenerExt::tap_io` appliqué au `TcpListener` avant `axum::serve`, et
   `socket2::SockRef`), activer `SO_KEEPALIVE`
   (inactivité 30 s, intervalle 10 s, 3 sondes) et `TCP_USER_TIMEOUT` = 30 s : le noyau
   ferme la connexion si des données restent non acquittées 30 s, hyper détruit alors le
   corps (désabonnement, libération du permis). Écarté : délai d'écriture applicatif par
   trame (nécessite un chien de garde par connexion, hyper ne sondant plus le corps quand
   l'écriture est bloquée). L'arrêt global reste borné par `shutdown_timeout`
   (voir [architecture](../architecture.md#54-arrêt-propre-sigterm)).
   **Complément (implémentation v1, 2026-11-26)** : les options sont appliquées par
   `axum::serve::ListenerExt::tap_io` (axum 0.8, confirmé) ; `TCP_NODELAY` est ajouté
   (`TcpOptions::nodelay`, vrai par défaut) pour émettre chaque partie MJPEG sans délai de
   Nagle ; une erreur d'application des options n'est pas fatale (connexion servie sans elles).
   Pour la décision 6, le limiteur est `server::mjpeg::ClientLimiter` et le permis est détenu
   par l'état du corps MJPEG (`ClientSession`).

`watch` reste une alternative valable pour les trames (sémantique « dernière image ») : si les
mesures sur Pi montrent une latence ou une mémoire excessive avec `broadcast`, le remplacement
est local à `FrameHub` (l'API publique `publish` / `subscribe` / `next_frame` ne change pas).

> **Complément (robustesse après revue v1, 2026-11-26)** :
>
> - les paniques de `v4l` **hors `Drop`** (`Capabilities::from` sur un nom de carte non UTF-8,
>   `Format::from` sur un champ inconnu, échec d'allocation mmap) sont interceptées dans
>   l'adaptateur (`guarded`) et deviennent `CaptureError::DriverFault`, classée **`Reopen`** et
>   non `Fatal` : la reprise n'a lieu qu'avec des spectateurs, avec attente exponentielle,
>   l'erreur reste visible dans `/health`, alors que `Fatal` provoquerait une boucle de
>   redémarrage Docker sans rien réparer ;
> - des trames invalides **consécutives** au-delà de `CAMSTREAM_MAX_CORRUPTED_FRAMES` (30 par
>   défaut) produisent `CaptureError::CorruptedStream` (`Reopen`) ;
> - à l'arrêt, `read_timeout < shutdown_timeout` est validé (code 2 sinon) et un join expiré
>   du thread de capture n'est plus compté comme un échec (seule une panique l'est) ;
> - un `compile_error!` interdit `panic = "abort"`, dont dépendent ces mécanismes.
>
> Voir [architecture § 7.2](../architecture.md#72-robustesse-après-la-revue-de-la-v1).
>
> **Revue d'architecture (2026-11-26)** — choix validés :
>
> - **`DriverFault` en `Reopen`** : `Fatal` transformerait un défaut de bibliothèque, souvent
>   transitoire (débranchement pendant `munmap`), en boucle de redémarrage du conteneur ; la
>   reprise est bornée (spectateurs présents, attente exponentielle plafonnée) et visible
>   (`/health` `reconnecting` avec le message). Limite acceptée : une cause permanente (nom de
>   carte non UTF-8) laisse les spectateurs sans image avec un `/health` à `200` ; le
>   diagnostic passe par le message d'erreur et les logs `error`.
> - **`CorruptedStream` dans le port `domain`** plutôt qu'un motif interne au superviseur :
>   l'issue « source défaillante » est un fait métier, classé par la même table
>   `CaptureError::recovery()` et transporté par le même chemin jusqu'à
>   `SourceStatus::Reconnecting { last_error }` ; un type interne dupliquerait ce chemin pour
>   un seul cas. Contrepartie : la variante n'est **jamais** produite par un adaptateur (le
>   seuil est une politique du superviseur, `SupervisorSettings::max_corrupted_frames`) ; cette
>   règle est écrite dans le commentaire de la variante et vérifiée en revue. Si d'autres
>   erreurs propres au superviseur apparaissent, elles seront regroupées dans un type
>   `stream` dédié (nouvel ADR).
> - **Join expiré non fatal à l'arrêt** (`CaptureEnd::TimedOut`) : cohérent avec la
>   validation `read_timeout < shutdown_timeout` ; seule une panique du thread de capture
>   donne un code 1 après un arrêt demandé.

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
