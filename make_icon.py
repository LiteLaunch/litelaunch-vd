"""Generates icon.png (results, 256 px) and vd.ico (notifications, 16-256 px): a white film frame
with a download arrow on an indigo rounded square, the same look as the other LiteLaunch icons."""
from pathlib import Path
from PIL import Image, ImageDraw

S = 1024
BG = (84, 76, 214, 255)
W = (255, 255, 255, 255)

img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
d = ImageDraw.Draw(img)
d.rounded_rectangle((32, 32, S - 32, S - 32), radius=220, fill=BG)

# Film frame: a white band with sprocket holes along the top and bottom.
d.rounded_rectangle((190, 250, 834, 774), radius=70, fill=W)
d.rounded_rectangle((270, 330, 754, 694), radius=36, fill=BG)
for x in range(232, 800, 96):
    d.rounded_rectangle((x, 268, x + 44, 306), radius=10, fill=BG)
    d.rounded_rectangle((x, 718, x + 44, 756), radius=10, fill=BG)

# Download arrow inside the frame.
d.rounded_rectangle((474, 372, 550, 540), radius=20, fill=W)
d.polygon([(400, 520), (624, 520), (512, 652)], fill=W)

here = Path(__file__).parent
img.resize((256, 256), Image.LANCZOS).save(here / "icon.png")
img.save(here / "vd.ico", sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (256, 256)])
print("ok")
