# 0002 — Transport vidéo : MJPEG en `multipart/x-mixed-replace`

- Statut : Proposé
- Date : 2026-11-26

## Contexte

Le flux de la webcam doit être affiché dans un navigateur, par plusieurs clients simultanés,
depuis un Raspberry Pi au CPU limité. La webcam fournit nativement du MJPEG (suite d'images
JPEG) dans la grande majorité des cas ; YUYV (brut) est le format de repli.

Le critère principal est le **coût CPU côté Raspberry** : tout ré-encodage (H.264, VP8…) est à
éviter. Les critères secondaires sont la simplicité côté navigateur et la latence.

## Options envisagées

### Option A — MJPEG via `multipart/x-mixed-replace` sur HTTP

Le serveur répond à `GET /stream.mjpg` par un corps infini composé de parties JPEG séparées
par une frontière (`boundary`). Le navigateur l'affiche dans une simple balise `<img>`.

- Avantages : **zéro ré-encodage** (les JPEG de la webcam sont envoyés tels quels), aucun
  JavaScript requis, supporté par tous les navigateurs courants, trivial à implémenter avec un
  corps HTTP en streaming, facile à déboguer (`curl`, VLC, ffplay).
- Inconvénients : débit élevé (JPEG ≈ 10 à 20 fois plus lourd que du H.264 à qualité égale),
  pas d'audio, limite d'environ 6 connexions HTTP/1.1 simultanées par hôte et par navigateur,
  pas de contrôle de congestion autre que TCP.

### Option B — WebSocket transportant des JPEG

Le serveur pousse des trames JPEG binaires ; un script les dessine dans un `<canvas>` ou une
`<img>` via `URL.createObjectURL`.

- Avantages : zéro ré-encodage également, canal bidirectionnel (commandes, statistiques),
  possibilité d'un contrôle de flux applicatif (accusés de réception).
- Inconvénients : JavaScript obligatoire, gestion du cycle de vie des `Blob` côté navigateur,
  davantage de code serveur, gain nul tant qu'aucune commande bidirectionnelle n'est requise.

### Option C — WebRTC (H.264/VP8)

- Avantages : latence très faible, débit réduit, contrôle de congestion intégré.
- Inconvénients : nécessite un encodage H.264/VP8 (coûteux sans encodeur matériel ; le Pi 5
  n'a plus d'encodeur H.264 matériel), signalisation, ICE/STUN, piles logicielles lourdes
  (`webrtc-rs`), complexité très supérieure. Disproportionné pour la première version.

### Option D — HLS / DASH

- Avantages : très compatible, adapté à la diffusion à grande échelle.
- Inconvénients : ré-encodage et segmentation, latence de plusieurs secondes. Hors sujet.

## Décision

**Option A** : MJPEG `multipart/x-mixed-replace; boundary=frame` sur HTTP, servi par
l'adaptateur `server`.

Points de conception :

- chaque partie est émise en **deux morceaux** (en-tête de partie construit, puis le `Bytes`
  du JPEG partagé) pour **ne jamais copier** l'image par client ;
- chaque partie porte `Content-Type: image/jpeg` et `Content-Length` ;
- en-têtes de réponse : `Cache-Control: no-cache, no-store`, `Connection: keep-alive` implicite
  (HTTP/1.1) ;
- un nombre maximal de clients simultanés est configurable (`CAMSTREAM_MAX_CLIENTS`) ; au-delà,
  réponse `503 Service Unavailable`.

> **Complément (implémentation v1, 2026-11-26)** : le CRLF qui termine chaque image est émis
> en tête de l'en-tête de la partie suivante (délimiteur `CRLF--frame` de la RFC 2046), ce qui
> garde deux morceaux par trame ; le refus au-delà de `CAMSTREAM_MAX_CLIENTS` porte
> `Retry-After: 5` ; `TCP_NODELAY` est activé sur chaque connexion (ADR 0004, décision 7) ; les
> trames sont vérifiées (`SOI`/`EOI`, bourrage final retiré) avant diffusion, sans décodage
> (`capture::jpeg`).

## Conséquences

- Le contrat du port `FrameSource` impose des trames **JPEG** : le format YUYV de repli doit
  être converti en JPEG dans l'adaptateur de capture (seul cas de ré-encodage, coûteux et
  donc limité au repli ; voir [architecture](../architecture.md#format-yuyv-de-repli)).
- Le débit réseau est le facteur limitant : résolution et FPS par défaut prudents
  (640x480 à 15 i/s).
- L'ajout ultérieur d'un transport WebSocket ou WebRTC se fera par un **nouvel adaptateur**
  abonné au `FrameHub`, sans modification du cœur ; WebRTC fera l'objet de son propre ADR
  (choix de l'encodeur notamment).
