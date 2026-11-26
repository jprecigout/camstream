# 0006 — Capture à la demande

- Statut : Proposé
- Date : 2026-11-26

## Contexte

L'utilisateur a décidé (2026-11-26) que la webcam doit être **arrêtée quand aucun client ne
regarde** et **démarrée au premier client**. Motivations : CPU, consommation et échauffement du
Raspberry Pi, usure de la webcam, et voyant de la webcam qui ne s'allume que lors d'un
visionnage réel.

Contraintes :

- le cœur (`stream`) ne doit pas dépendre du transport HTTP : la notion de « client » doit
  rester valable pour un futur transport WebSocket ou WebRTC ;
- le superviseur s'exécute dans un thread OS bloquant ([ADR 0004](0004-distribution-des-trames-et-concurrence.md)) :
  il doit pouvoir **attendre** un premier client sans attente active, et rester interruptible
  par l'arrêt ;
- un rechargement de page (fermeture puis réouverture de la connexion en moins d'une seconde)
  ne doit pas fermer puis rouvrir la webcam ;
- l'ouverture d'une webcam UVC (ouverture, négociation, `STREAMON`, première trame) prend
  typiquement 0,3 à 1,5 s ;
- les erreurs de configuration (droits, format) doivent apparaître **au démarrage du
  conteneur**, pas seulement à l'arrivée du premier spectateur ;
- la reconnexion avec attente exponentielle et `/health` doivent rester cohérents.

## Options envisagées

### Qui compte les clients ?

| Option | Avantages | Inconvénients |
|---|---|---|
| `broadcast::Sender::receiver_count()` interrogé périodiquement | Aucun code supplémentaire dans le hub | Aucune notification de changement : il faut sonder (latence au premier client ou réveils inutiles en veille) |
| Le serveur (`ClientSlot` / `Semaphore`) prévient le superviseur | Le serveur connaît déjà ses clients | Couple la demande au transport HTTP ; chaque futur transport devrait reproduire la logique ; `server` devrait connaître le superviseur |
| **Compteur d'abonnés tenu par `FrameHub`, publié par `watch<usize>`** | Transport-agnostique (tout abonné compte) ; incrément dans `subscribe`, décrément dans le `Drop` de `FrameSubscriber` (**RAII**, impossible d'oublier) ; `watch` fournit à la fois la valeur courante et la notification | Un `Arc<watch::Sender<usize>>` de plus dans le hub et dans chaque abonné |
| `Notify` | Léger | Réveil sans état : il faut quand même un compteur, et gérer les réveils perdus ; `watch` couvre les deux besoins |

### Que faire quand le dernier client part ?

| Option | Avantages | Inconvénients |
|---|---|---|
| Arrêt immédiat | Économie maximale | Chaque rechargement ferme et rouvre la webcam (1 s d'écran vide, usure, logs bruyants) |
| **Délai de grâce configurable, capture maintenue pendant le délai** | Un rechargement retrouve une webcam déjà chaude (première image immédiate) | Quelques secondes de capture « pour rien » |
| Pause du streaming (`STREAMOFF`) en gardant le périphérique ouvert | Redémarrage un peu plus rapide | API `v4l` 0.14 : le flux mmap s'arrête à sa destruction, pas de pause exposée ; gain marginal face au délai de grâce ; le voyant peut rester allumé selon la webcam |

### Latence du premier client

| Option | Avantages | Inconvénients |
|---|---|---|
| **Envoyer `200` et les en-têtes multipart tout de suite, les trames suivent** | Simple ; comportement identique à `Reconnecting` (connexion ouverte, images quand elles arrivent) ; aucune temporisation dans le handler | Image vide pendant le démarrage (≈ 1 s) |
| Attendre la première trame avant de répondre (avec délai maximal, sinon `503`) | Le navigateur reçoit tout de suite une image | Lie la réponse HTTP à l'état de la caméra ; temporisation et cas d'erreur supplémentaires ; le `503` pendant une reconnexion contredit le comportement voulu |

## Décision

1. **Demande = nombre de `FrameSubscriber` vivants**, tenu par `FrameHub` et publié par un
   `watch<usize>` ; le superviseur l'observe via `DemandReceiver` (obtenu par
   `FrameHub::demand()`). Le `Semaphore` du serveur reste une **limite** propre au transport
   HTTP (`MaxClients`), indépendante de la demande.
2. **Délai de grâce `CAMSTREAM_IDLE_TIMEOUT_MS`, défaut 10 000 ms** (10 s) : couvre largement
   un rechargement de page ou une navigation aller-retour, pour un coût négligeable. Pendant
   le délai, la capture continue (les trames sont jetées, `publish` renvoie 0). Valeur `0` :
   arrêt immédiat. À l'expiration, la source est **détruite** (RAII : `STREAMOFF`, `munmap`,
   fermeture du descripteur) et l'état passe à `Idle`.
3. **Sonde au démarrage** : au lancement, le superviseur crée la source une fois puis la
   détruit aussitôt s'il n'y a aucun abonné. Une erreur `Fatal` (droits, format) arrête
   l'application immédiatement (`Failed`, code 1) ; une erreur `Reopen` (webcam absente) est
   journalisée en `warn` et l'état passe à `Idle` : on ne réessaie qu'en présence de demande.
4. **Premier client** : le handler s'abonne **avant** de répondre (la demande est donc
   visible immédiatement), puis envoie `200` et les en-têtes ; les parties JPEG suivent dès
   que la webcam produit. La page HTML affiche un texte d'attente jusqu'au premier
   `onload` de l'image.
5. **Interaction avec la reconnexion** : on ne tente de (re)créer la source que si la demande
   est non nulle. L'attente exponentielle est interrompue par le jeton d'arrêt **ou** par le
   passage de la demande à 0 (→ `Idle`, compteur de tentatives remis à zéro). Au retour d'une
   demande depuis `Idle`, la première tentative est immédiate.
6. **Attentes du thread de capture** : toutes les attentes (premier client, délai de grâce,
   attente exponentielle) passent par `tokio::runtime::Handle::block_on` sur un `select!`
   (jeton, demande, minuterie). Le `Handle` est injecté au superviseur par `app`.
   `Handle::block_on` est autorisé depuis un thread hors runtime, ce qui est le cas.
7. **`/health`** : `Idle` est un état sain → `200`, `"state": "idle"` dans le JSON.

## Conséquences

- Nouvel état `SourceStatus::Idle` et nouveaux types `DemandReceiver`, `SupervisorSettings`
  ([architecture](../architecture.md#34-stream--diffuseur-état-et-superviseur)).
- Testable sans matériel : une fabrique de test compte les appels à `create()` et les
  destructions de source ; abonnement / désabonnement avec un délai de grâce court (50 ms).
- Un client disparu sans fermer sa connexion maintiendrait la webcam allumée : d'où
  `TCP_USER_TIMEOUT`/keepalive (ADR 0004, décision 7).
- Sans spectateur, la webcam n'est pas surveillée : un débranchement n'est détecté qu'à
  l'arrivée du client suivant (état `Reconnecting` à ce moment-là). Accepté.
- Le délai d'ouverture d'une webcam réelle et le comportement de son voyant sont à mesurer
  pendant le spike `v4l` ; le défaut de 10 s pourra être ajusté.
