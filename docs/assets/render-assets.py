#!/usr/bin/env python3
"""Regenerate the README banner and the GitHub social preview.

    python3 docs/assets/render-assets.py

Both images reuse the mark, palette and JetBrains Mono ExtraBold outlines from
site/render-brand.py, so they match the site's og.png and twitter.png. Text is
filled from font outlines in code; nothing here is model-generated.

Needs fonttools (pip install fonttools), like render-brand.py.

Outputs, next to this script:
  banner.png          1600x400, the wide README header
  social-preview.png  1280x640, the GitHub repository social preview
"""

from __future__ import annotations

import importlib.util
from pathlib import Path

HERE = Path(__file__).resolve().parent
BRAND = HERE.parent.parent / "site" / "render-brand.py"

spec = importlib.util.spec_from_file_location("render_brand", BRAND)
if spec is None or spec.loader is None:
    raise SystemExit(f"cannot load {BRAND}")
brand = importlib.util.module_from_spec(spec)
spec.loader.exec_module(brand)

HEADLINE = "Many agents"
DETAIL = "one project."
# What is on main, in the order the README leads with. Kept short so the
# banner stays legible at README width.
LINE = "Coordinate work on one machine."


def banner(text_width, draw_text) -> brand.Canvas:
    width, height = 1600, 400
    canvas = brand.Canvas(width, height, brand.PAPER, alpha=255)
    mark = 176
    word_size, headline_size, detail_size, line_size = 40, 112, 60, 30
    gap_word, gap_head, gap_line, gap_mark = 14, 10, 22, 56
    headline_w = text_width(HEADLINE + " " + DETAIL, headline_size, 0)
    if headline_w > width * 0.62:
        # Two lines: headline, then detail.
        block_h = (
            brand._line_height(word_size) + gap_word + brand._line_height(headline_size)
            + gap_head + brand._line_height(detail_size) + gap_line + brand._line_height(line_size)
        )
    else:
        block_h = brand._line_height(word_size) + gap_word + brand._line_height(headline_size) + gap_line + brand._line_height(line_size)
    bars_w = (12 / brand.TILE) * mark
    left = 120
    top = (height - block_h) / 2
    grid_x = left - (2 / brand.TILE) * mark
    grid_y = top + (block_h - mark) / 2
    brand.paint_mark(canvas, grid_x, grid_y, mark, tile=False)
    text_x = left + bars_w + gap_mark
    y = top
    y = brand._draw_line(draw_text, canvas, brand.WORDMARK, text_x, y, word_size, -0.03)
    y += gap_word
    if headline_w > width * 0.62:
        y = brand._draw_line(draw_text, canvas, HEADLINE, text_x, y, headline_size, 0)
        y += gap_head
        y = brand._draw_line(draw_text, canvas, DETAIL, text_x, y, detail_size, 0)
    else:
        y = brand._draw_line(draw_text, canvas, HEADLINE + " " + DETAIL, text_x, y, headline_size, 0)
    y += gap_line
    line_w = text_width(LINE, line_size, 0)
    if text_x + line_w > width - 60:
        raise SystemExit(f"banner detail line is {line_w:.0f}px wide and runs off the frame")
    brand._draw_line(draw_text, canvas, LINE, text_x, y, line_size, 0)
    return canvas


def social(text_width, draw_text) -> brand.Canvas:
    """1280x640 (2:1). GitHub shows the whole frame; keep a 48px safe margin."""
    width, height = 1280, 640
    canvas = brand.Canvas(width, height, brand.PAPER, alpha=255)
    mark = 120
    word_size, headline_size, detail_size = 52, 140, 88
    gap_mark, gap_word, gap_head = 18, 34, 18
    headline_w = text_width(HEADLINE, headline_size, 0)
    detail_w = text_width(DETAIL, detail_size, 0)
    if headline_w < width * 0.5:
        raise SystemExit(f"headline is only {headline_w:.0f}px wide")
    block = (
        mark + gap_mark + brand._line_height(word_size) + gap_word
        + brand._line_height(headline_size) + gap_head + brand._line_height(detail_size)
    )
    top = (height - block) / 2
    brand.paint_mark(canvas, (width - mark) / 2, top, mark, tile=False)
    y = top + mark + gap_mark
    word_w = text_width(brand.WORDMARK, word_size, -0.03)
    y = brand._draw_line(draw_text, canvas, brand.WORDMARK, (width - word_w) / 2, y, word_size, -0.03)
    y += gap_word
    y = brand._draw_line(draw_text, canvas, HEADLINE, (width - headline_w) / 2, y, headline_size, 0)
    y += gap_head
    brand._draw_line(draw_text, canvas, DETAIL, (width - detail_w) / 2, y, detail_size, 0)
    margin = 48
    paper = bytes(brand.PAPER)
    for yy in range(height):
        row = yy * width * 4
        for xx in range(width):
            i = row + xx * 4
            if canvas.buf[i : i + 3] != paper and (xx < margin or xx >= width - margin or yy < margin or yy >= height - margin):
                raise SystemExit(f"social preview content touches the margin at {xx},{yy}")
    return canvas


def main() -> None:
    text_width, draw_text = brand.load_font()
    b = banner(text_width, draw_text)
    brand.write_png(HERE / "banner.png", b.width, b.height, b.bytes())
    s = social(text_width, draw_text)
    brand.write_png(HERE / "social-preview.png", s.width, s.height, s.bytes())
    print(f"wrote banner.png ({b.width}x{b.height}) and social-preview.png ({s.width}x{s.height}) to {HERE}")


if __name__ == "__main__":
    main()
