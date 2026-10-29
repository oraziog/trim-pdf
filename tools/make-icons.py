"""Dai due disegni di partenza alle icone che finiscono nel repo e negli eseguibili.

    python tools/make-icons.py assets/icona-cli-master.png assets/icona-gui-master.png

I due argomenti sono le immagini di partenza (PNG o JPG, almeno 512x512). Lo sfondo bianco
attorno al cerchio (CLI) e fuori dagli angoli arrotondati (GUI) diventa trasparente: cosi'
l'icona sta bene sia su una barra delle applicazioni chiara sia su una scura.

Produce:

    assets/icona-cli.png         512x512, l'icona della console
    assets/icona-gui.png         512x512, l'icona della finestra (e quella del README)
    assets/icona-cli.ico         16, 24, 32, 48, 64, 128, 256 px
    assets/icona-gui.ico         le stesse misure
    assets/social-preview.png    1280x640, da caricare nelle impostazioni di GitHub

Pillow serve solo a questo script, non alla compilazione.
"""

import sys
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
SIZES = (256, 128, 64, 48, 32, 24, 16)


def unwhite(image: Image.Image, tolerance: int = 24) -> Image.Image:
    """Rende trasparente il bianco che tocca i bordi, senza toccare il bianco interno.

    Si allaga dai quattro angoli invece di sostituire ogni pixel bianco: la pagina disegnata
    e' quasi bianca, e un filtro per colore la bucherellerebbe.
    """
    image = image.convert("RGBA")
    width, height = image.size
    pixels = image.load()
    seen = bytearray(width * height)
    stack = [(0, 0), (width - 1, 0), (0, height - 1), (width - 1, height - 1)]
    while stack:
        x, y = stack.pop()
        if not (0 <= x < width and 0 <= y < height) or seen[y * width + x]:
            continue
        red, green, blue, alpha = pixels[x, y]
        if alpha == 0 or min(red, green, blue) < 255 - tolerance:
            continue
        seen[y * width + x] = 1
        pixels[x, y] = (0, 0, 0, 0)
        stack.extend(((x + 1, y), (x - 1, y), (x, y + 1), (x, y - 1)))
    return image


def squared(image: Image.Image) -> Image.Image:
    """Porta l'immagine a un quadrato, tagliando il lato piu' lungo dal centro."""
    width, height = image.size
    side = min(width, height)
    left = (width - side) // 2
    top = (height - side) // 2
    return image.crop((left, top, left + side, top + side))


def main(arguments: list[str]) -> int:
    if len(arguments) != 2:
        print(__doc__)
        return 2

    (ROOT / "assets").mkdir(exist_ok=True)

    masters = {}
    for name, source in zip(("cli", "gui"), arguments):
        image = squared(Image.open(source)).resize((512, 512), Image.LANCZOS)
        image = unwhite(image)
        master = ROOT / "assets" / f"icona-{name}.png"
        image.save(master, optimize=True)
        masters[name] = image
        print(f"{master.relative_to(ROOT)}  {master.stat().st_size} B")

    for name, image in masters.items():
        target = ROOT / "assets" / f"icona-{name}.ico"
        frames = [image.resize((size, size), Image.LANCZOS) for size in SIZES]
        # Il 256 va dentro l'ico come PNG compresso, come fanno tutti gli altri programmi.
        frames[0].save(
            target,
            format="ICO",
            sizes=[(size, size) for size in SIZES],
            append_images=frames[1:],
        )
        print(f"{target.relative_to(ROOT)}  {target.stat().st_size} B")

    social = Image.new("RGBA", (1280, 640), (27, 27, 27, 255))
    badge = masters["gui"].resize((384, 384), Image.LANCZOS)
    social.alpha_composite(badge, (96, (640 - 384) // 2))
    # Una riga arancione sotto lo spazio lasciato libero per il titolo.
    social.paste((230, 126, 34, 255), (560, 424, 1184, 428))
    target = ROOT / "assets" / "social-preview.png"
    social.convert("RGB").save(target, optimize=True)
    print(f"{target.relative_to(ROOT)}  {target.stat().st_size} B")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
