# 0007 — Sécurité de la v1 : réseau local, sans authentification ni TLS

- Statut : Proposé
- Date : 2026-11-26

## Contexte

Camstream diffuse l'image d'une webcam : une exposition involontaire sur Internet serait une
atteinte directe à la vie privée. L'utilisateur a décidé (2026-11-26) d'un usage **sur le
réseau local uniquement** pour la v1, sans authentification ni TLS.

Points techniques à prendre en compte :

- dans un conteneur, l'application doit écouter sur `0.0.0.0` pour que la publication de port
  Docker fonctionne ; c'est la publication (`-p` / `ports:`) qui détermine l'exposition réelle
  sur l'hôte ;
- Docker insère ses propres règles `iptables` : un port publié **contourne** les pare-feu de
  type `ufw` de l'hôte ;
- les points d'accès sont tous en lecture (`GET`) ; aucune donnée n'est reçue ni stockée.

## Options envisagées

| Option | Avantages | Inconvénients |
|---|---|---|
| **Aucune authentification, usage LAN, exposition maîtrisée par la configuration réseau** | Aucun code de sécurité à maintenir ; aucune dépendance (TLS, hachage) ; compatible avec la balise `<img>` | Toute personne sur le LAN voit le flux ; repose sur la discipline de déploiement |
| Authentification HTTP Basic intégrée | Simple pour le navigateur | Sans TLS, le mot de passe circule en clair sur le LAN : fausse sécurité ; gestion du secret |
| TLS intégré (`rustls`) + authentification | Autonome | Certificats sur un Pi (renouvellement, CA locale) ; dépendances et CPU ; doublon d'un reverse proxy |
| Reverse proxy en amont (TLS + authentification) | Fonctions éprouvées, centralisées, sans code dans Camstream | Composant supplémentaire à déployer |

## Décision

- **v1 : aucune authentification ni TLS dans Camstream**, usage réservé au réseau local.
- **Défaut d'écoute sûr hors conteneur** : `CAMSTREAM_BIND` vaut `127.0.0.1` par défaut
  (`cargo run` n'expose rien au réseau) ; l'image Docker fixe `CAMSTREAM_BIND=0.0.0.0`, et
  l'exposition est restreinte au niveau de la publication de port.
- **Recommandations de déploiement** (documentées dans [deploiement.md](../deploiement.md#9-sécurité)) :
  publier le port sur l'adresse LAN du Pi (`"192.168.1.20:8080:8080"`) plutôt que sur toutes
  les interfaces ; **ne jamais** créer de redirection de port sur la box/le routeur ni
  d'exposition UPnP ; ne pas compter sur `ufw` pour filtrer un port publié par Docker.
- **Durcissement conservé** : utilisateur non root, `cap_drop: ALL`, `no-new-privileges`,
  système de fichiers en lecture seule, uniquement des routes `GET`, limite de clients.
- **Évolution** : un accès hors LAN passera par un reverse proxy assurant TLS et
  authentification, ou par un VPN vers le réseau local ; Camstream restera alors joignable
  seulement par le proxy (réseau Docker interne ou `127.0.0.1`). Ce choix fera l'objet d'un
  nouvel ADR remplaçant celui-ci.

> **Complément (implémentation v1, 2026-11-26)** : vérifié dans le code — `CAMSTREAM_BIND`
> vaut `127.0.0.1` par défaut (`config/cli.rs`), l'image fixe `0.0.0.0` ; le routeur ne déclare
> que des routes `GET` (`/`, `/stream.mjpg`, `/health`) ; la sonde `camstream healthcheck`
> interroge la boucle locale quand l'écoute est `0.0.0.0`/`::` ; le `docker-compose.yml`
> publie le port sur une adresse LAN d'exemple.

## Conséquences

- Le `README` et la page d'accueil n'annoncent aucune protection : la documentation de
  déploiement porte un avertissement explicite.
- Aucune dépendance cryptographique en v1.
- Le passage derrière un reverse proxy ne demande aucune modification du code (en-têtes
  `X-Forwarded-*` non utilisés) ; seul un éventuel préfixe de chemin serait à prévoir.
