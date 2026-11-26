// Script de la page d'accueil de Camstream (servi par GET /camstream.js).
// Il ne fait que masquer le message d'attente dès la première image, et relancer
// le flux si la connexion tombe (redémarrage, Wi-Fi). Chargé avec « defer » :
// le DOM est prêt quand il s'exécute.
"use strict";

const img = document.getElementById("stream");
const message = document.getElementById("message");
let retry = null;
let watch = null;

function shown() {
  img.classList.remove("waiting");
  message.hidden = true;
  // Première image affichée : la surveillance périodique n'a plus d'utilité.
  clearInterval(watch);
}

img.addEventListener("load", shown);
// Selon le navigateur, « load » n'est pas toujours émis pour un flux multipart (ni
// avant l'ajout de cet écouteur) : on surveille aussi l'apparition de la première image,
// jusqu'à ce qu'elle soit affichée (puis de nouveau après chaque relance du flux).
function watchFirstImage() {
  clearInterval(watch);
  watch = setInterval(() => { if (img.naturalWidth > 0) shown(); }, 250);
}
watchFirstImage();

function failed() {
  img.classList.add("waiting");
  message.hidden = false;
  message.textContent = "Connexion au flux perdue, nouvelle tentative…";
  clearTimeout(retry);
  retry = setTimeout(() => {
    img.src = "/stream.mjpg?t=" + Date.now();
    watchFirstImage();
  }, 3000);
}

img.addEventListener("error", failed);
// Avec « defer », l'image commence à charger avant l'exécution du script : un échec
// immédiat (503 au-delà de la limite de clients, serveur absent) peut précéder l'ajout
// de l'écouteur. Une image « complete » sans dimensions est une image en échec.
if (img.complete && img.naturalWidth === 0) failed();
