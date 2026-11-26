#!/usr/bin/env python3
"""Génère les images JPEG de la mire synthétique de Camstream.

Les images sont embarquées dans le binaire par `include_bytes!`
(`src/capture/synthetic.rs`) : aucun encodage n'a lieu à l'exécution.
Usage (depuis la racine du dépôt, Pillow requis) :

    python3 scripts/generate_synthetic_frames.py

Le script est déterministe : relancé, il produit les mêmes fichiers.
"""

from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

WIDTH, HEIGHT = 640, 480
FRAME_COUNT = 10
QUALITY = 75
OUTPUT = Path(__file__).resolve().parent.parent / "assets" / "synthetic"

# Barres de couleur classiques (blanc, jaune, cyan, vert, magenta, rouge, bleu).
BARS = [
    (192, 192, 192),
    (192, 192, 0),
    (0, 192, 192),
    (0, 192, 0),
    (192, 0, 192),
    (192, 0, 0),
    (0, 0, 192),
]


def frame(index: int) -> Image.Image:
    image = Image.new("RGB", (WIDTH, HEIGHT), (16, 16, 16))
    draw = ImageDraw.Draw(image)
    bar_width = WIDTH / len(BARS)
    for i, color in enumerate(BARS):
        draw.rectangle(
            [round(i * bar_width), 0, round((i + 1) * bar_width) - 1, HEIGHT * 2 // 3],
            fill=color,
        )
    # Carré mobile : rend le mouvement (et donc le rafraîchissement) visible.
    size = 48
    x = round(index * (WIDTH - size) / (FRAME_COUNT - 1))
    y = HEIGHT * 2 // 3 + 24
    draw.rectangle([x, y, x + size - 1, y + size - 1], fill=(255, 255, 255))
    font = ImageFont.load_default(size=28)
    draw.text((16, HEIGHT - 64), "Camstream - mire de test", fill=(235, 235, 235), font=font)
    draw.text(
        (WIDTH - 96, HEIGHT - 64), f"{index + 1:02d}/{FRAME_COUNT}", fill=(235, 235, 235), font=font
    )
    return image


def main() -> None:
    OUTPUT.mkdir(parents=True, exist_ok=True)
    for index in range(FRAME_COUNT):
        path = OUTPUT / f"frame-{index:02d}.jpg"
        frame(index).save(path, "JPEG", quality=QUALITY, optimize=True)
        print(f"{path.name}: {path.stat().st_size} octets")


if __name__ == "__main__":
    main()
