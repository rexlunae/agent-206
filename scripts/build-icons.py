#!/usr/bin/env python3
"""Derive bundle icons from the Agent 206 vector mark. Requires cairosvg and Pillow."""
from pathlib import Path
import io
import cairosvg
from PIL import Image

assets = Path(__file__).resolve().parents[1] / 'crates/ui/assets'
image = Image.open(io.BytesIO(cairosvg.svg2png(url=str(assets / 'agent206.svg'), output_width=1024, output_height=1024))).convert('RGBA')
image.save(assets / 'agent206.png')
image.save(assets / 'agent206.ico', sizes=[(size, size) for size in (16, 24, 32, 48, 64, 128, 256)])
image.save(assets / 'agent206.icns')
print('Generated PNG, ICO and ICNS from agent206.svg')
