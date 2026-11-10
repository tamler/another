#!/usr/bin/env python3
"""Regenerate the site icon set and the social images.

    python3 site/render-brand.py

The mark, the words, and the layout are in this file. Card text is the
vendored JetBrains Mono ExtraBold, filled from its outlines, so the PNG bytes
do not depend on FreeType or on which machine runs it.

Requires fontTools:

    pip install fonttools

Outputs, next to this script, are the public files the site build copies:
favicon.svg, favicon.ico (16, 32, 48), apple-touch-icon.png (180),
icon-192.png, icon-512.png, site.webmanifest, og.png, twitter.png.
"""

from __future__ import annotations

import argparse
import json
import math
import struct
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent
CARD_FONT = ROOT / "fonts" / "jetbrains" / "JetBrainsMono-ExtraBold.ttf"

# Site palette: paper, ink, and the ochre used for links and the signal square.
PAPER = (0xF8, 0xF8, 0xF4)
INK = (0x29, 0x29, 0x22)
OCHRE = (0x70, 0x50, 0x00)

# 16px grid. Two agents stand on a shared baseline. The ochre square is the
# message between them, low, with a one-pixel gap on each side so the three
# shapes stay separate at 16px. Outer corners of the favicon tile use rx=2.
AGENT_LEFT = (2, 3, 3, 10)
AGENT_RIGHT = (11, 3, 3, 10)
MESSAGE = (6, 9, 4, 4)
TILE = 16
TILE_RADIUS = 2

WORDMARK = "another"
# Short on purpose. At feed size a long line becomes unreadably small.
HEADLINE = "One inbox"
DETAIL = "for coding agents."

# Fragment Mono's advance is 618/1000 for every glyph, including the space.
# Filled in by load_font from JetBrains Mono ExtraBold.
UPM = 1000
ADVANCE = 600
CAP = 0.73
DESC = 0.30


def write_png(path: Path, width: int, height: int, rgba: bytes) -> None:
    if len(rgba) != width * height * 4:
        raise SystemExit(f"png buffer {path} is {len(rgba)} bytes, expected {width * height * 4}")
    raw = bytearray()
    stride = width * 4
    for y in range(height):
        raw.append(0)
        raw += rgba[y * stride : (y + 1) * stride]
    compressed = zlib.compress(bytes(raw), 9)

    def chunk(tag: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    path.write_bytes(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", compressed) + chunk(b"IEND", b""))


def png_bytes(width: int, height: int, rgba: bytes) -> bytes:
    raw = bytearray()
    stride = width * 4
    for y in range(height):
        raw.append(0)
        raw += rgba[y * stride : (y + 1) * stride]
    compressed = zlib.compress(bytes(raw), 9)

    def chunk(tag: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", compressed) + chunk(b"IEND", b"")


def write_ico(path: Path, images: list[tuple[int, int, bytes]]) -> None:
    count = len(images)
    header = struct.pack("<HHH", 0, 1, count)
    entries = bytearray()
    blobs = bytearray()
    offset = 6 + 16 * count
    for width, height, blob in images:
        entries += struct.pack(
            "<BBBBHHII",
            width if width < 256 else 0,
            height if height < 256 else 0,
            0,
            0,
            1,
            32,
            len(blob),
            offset,
        )
        offset += len(blob)
        blobs += blob
    path.write_bytes(header + entries + blobs)


class Canvas:
    def __init__(self, width: int, height: int, color: tuple[int, int, int], alpha: int = 255):
        self.width = width
        self.height = height
        pixel = bytes((color[0], color[1], color[2], alpha))
        self.buf = bytearray(pixel * (width * height))

    def blend(self, x: int, y: int, color: tuple[int, int, int], coverage: int) -> None:
        if coverage <= 0 or not (0 <= x < self.width and 0 <= y < self.height):
            return
        i = (y * self.width + x) * 4
        if coverage >= 255:
            self.buf[i] = color[0]
            self.buf[i + 1] = color[1]
            self.buf[i + 2] = color[2]
            self.buf[i + 3] = 255
            return
        a = coverage / 255.0
        inv = 1.0 - a
        dest_a = self.buf[i + 3] / 255.0
        out_a = a + dest_a * inv
        if out_a <= 0:
            return
        for channel in range(3):
            premul = color[channel] * a + self.buf[i + channel] * dest_a * inv
            self.buf[i + channel] = int(premul / out_a + 0.5)
        self.buf[i + 3] = int(out_a * 255 + 0.5)

    def fill_rect(self, x: int, y: int, w: int, h: int, color: tuple[int, int, int]) -> None:
        x0 = max(0, x)
        y0 = max(0, y)
        x1 = min(self.width, x + w)
        y1 = min(self.height, y + h)
        px = bytes((color[0], color[1], color[2], 255))
        for yy in range(y0, y1):
            row = yy * self.width * 4
            for xx in range(x0, x1):
                self.buf[row + xx * 4 : row + xx * 4 + 4] = px

    def bytes(self) -> bytes:
        return bytes(self.buf)


def _sample_box(x: float, scale: float) -> int:
    return int(round(x / TILE * scale))


def render_mark(size: int, samples: int = 4, tile: bool = True) -> tuple[bytearray, bytearray, bytearray]:
    """Coverage masks (0-255) for the paper tile, the agents, and the message."""
    scale = size * samples
    paper = bytearray(scale * scale)
    ink = bytearray(scale * scale)
    ochre = bytearray(scale * scale)

    def fill(mask: bytearray, box: tuple[int, int, int, int]) -> None:
        x, y, w, h = box
        x0 = _sample_box(x, scale)
        y0 = _sample_box(y, scale)
        x1 = _sample_box(x + w, scale)
        y1 = _sample_box(y + h, scale)
        for yy in range(y0, y1):
            row = yy * scale
            mask[row + x0 : row + x1] = b"\x01" * (x1 - x0)

    if tile:
        radius = _sample_box(TILE_RADIUS, scale)
        for yy in range(scale):
            row = yy * scale
            for xx in range(scale):
                dx = dy = 0
                if xx < radius and yy < radius:
                    dx, dy = radius - xx, radius - yy
                elif xx >= scale - radius and yy < radius:
                    dx, dy = xx - (scale - radius - 1), radius - yy
                elif xx < radius and yy >= scale - radius:
                    dx, dy = radius - xx, yy - (scale - radius - 1)
                elif xx >= scale - radius and yy >= scale - radius:
                    dx, dy = xx - (scale - radius - 1), yy - (scale - radius - 1)
                if dx or dy:
                    # Sample center sits half a sample in from the corner test above.
                    if dx * dx + dy * dy > radius * radius:
                        continue
                paper[row + xx] = 1
    fill(ink, AGENT_LEFT)
    fill(ink, AGENT_RIGHT)
    fill(ochre, MESSAGE)

    def down(mask: bytearray) -> bytearray:
        out = bytearray(size * size)
        area = samples * samples
        for y in range(size):
            for x in range(size):
                total = 0
                for sy in range(samples):
                    row = (y * samples + sy) * scale + x * samples
                    total += sum(mask[row : row + samples])
                out[y * size + x] = (total * 255 + area // 2) // area
        return out

    return down(paper), down(ink), down(ochre)


def paint_mark(canvas: Canvas, origin_x: float, origin_y: float, size: int, tile: bool) -> None:
    paper, ink, ochre = render_mark(size, samples=4, tile=tile)
    x0 = int(round(origin_x))
    y0 = int(round(origin_y))
    for y in range(size):
        for x in range(size):
            i = y * size + x
            if tile and paper[i]:
                canvas.blend(x0 + x, y0 + y, PAPER, paper[i])
            if ink[i]:
                canvas.blend(x0 + x, y0 + y, INK, ink[i])
            if ochre[i]:
                canvas.blend(x0 + x, y0 + y, OCHRE, ochre[i])


def load_font():
    global UPM, ADVANCE, CAP, DESC
    if not CARD_FONT.is_file():
        raise SystemExit(f"missing {CARD_FONT}")
    try:
        from fontTools.pens.recordingPen import DecomposingRecordingPen
        from fontTools.ttLib import TTFont
    except ImportError as exc:
        raise SystemExit("Install fonttools, then run this script again.") from exc
    try:
        font = TTFont(CARD_FONT)
    except Exception as exc:
        raise SystemExit("Could not read JetBrains Mono ExtraBold.") from exc
    if font["OS/2"].usWeightClass < 800:
        raise SystemExit("card font is not ExtraBold or heavier")
    glyph_set = font.getGlyphSet()
    cmap = font.getBestCmap()
    UPM = font["head"].unitsPerEm
    ADVANCE = glyph_set[cmap[ord(" ")]].width
    CAP = font["OS/2"].sCapHeight / UPM
    DESC = abs(font["OS/2"].sTypoDescender) / UPM
    outlines: dict[str, tuple[list[list[tuple[float, float]]], int]] = {}

    def flatten_quadratic(p0, p1, p2, out, depth: int = 0) -> None:
        mid = ((p0[0] + 2 * p1[0] + p2[0]) / 4, (p0[1] + 2 * p1[1] + p2[1]) / 4)
        chord = ((p0[0] + p2[0]) / 2, (p0[1] + p2[1]) / 2)
        if depth >= 8 or (mid[0] - chord[0]) ** 2 + (mid[1] - chord[1]) ** 2 <= 0.35:
            out.append(p2)
            return
        ab = ((p0[0] + p1[0]) / 2, (p0[1] + p1[1]) / 2)
        bc = ((p1[0] + p2[0]) / 2, (p1[1] + p2[1]) / 2)
        split = ((ab[0] + bc[0]) / 2, (ab[1] + bc[1]) / 2)
        flatten_quadratic(p0, ab, split, out, depth + 1)
        flatten_quadratic(split, bc, p2, out, depth + 1)

    def contours_for(char: str) -> list[list[tuple[float, float]]]:
        name = cmap.get(ord(char))
        if name is None:
            raise SystemExit(f"JetBrains Mono ExtraBold has no glyph for {char!r}")
        pen = DecomposingRecordingPen(glyph_set)
        glyph_set[name].draw(pen)
        contours: list[list[tuple[float, float]]] = []
        current: list[tuple[float, float]] = []
        start: tuple[float, float] | None = None
        for op, pts in pen.value:
            if op == "moveTo":
                if current:
                    contours.append(current)
                start = pts[0]
                current = [start]
            elif op == "lineTo":
                current.append(pts[0])
            elif op == "qCurveTo":
                points = list(pts)
                if points[-1] is None:
                    if start is None:
                        raise SystemExit(f"open quadratic in {char!r}")
                    points[-1] = start
                end = points[-1]
                offs = points[:-1]
                on = current[-1]
                if len(offs) == 1:
                    flatten_quadratic(on, offs[0], end, current)
                else:
                    for index in range(len(offs) - 1):
                        implied = (
                            (offs[index][0] + offs[index + 1][0]) / 2,
                            (offs[index][1] + offs[index + 1][1]) / 2,
                        )
                        flatten_quadratic(on, offs[index], implied, current)
                        on = implied
                    flatten_quadratic(on, offs[-1], end, current)
            elif op == "closePath":
                if current:
                    contours.append(current)
                    current = []
            else:
                raise SystemExit(f"unexpected outline op {op} in {char!r}")
        if current:
            contours.append(current)
        return contours, glyph_set[name].width

    def text_width(text: str, size: float, tracking: float) -> float:
        width = 0.0
        for char in text:
            if char not in outlines:
                outlines[char] = contours_for(char)
            width += size * (outlines[char][1] / UPM + tracking)
        return width

    def draw_text(
        canvas: Canvas,
        text: str,
        x: float,
        baseline: float,
        size: float,
        tracking: float,
        samples: int = 4,
    ) -> None:
        if not text:
            return
        scale = size / UPM
        polygons: list[list[tuple[float, float]]] = []
        pen_x = x
        for char in text:
            if char not in outlines:
                outlines[char] = contours_for(char)
            contours, advance = outlines[char]
            for contour in contours:
                polygons.append([(pen_x + px * scale, baseline - py * scale) for px, py in contour])
            pen_x += size * (advance / UPM + tracking)
        xs = [px for poly in polygons for px, _py in poly]
        ys = [py for poly in polygons for _px, py in poly]
        if not xs:
            return
        x0 = max(0, int(math.floor(min(xs))) - 1)
        y0 = max(0, int(math.floor(min(ys))) - 1)
        x1 = min(canvas.width, int(math.ceil(max(xs))) + 2)
        y1 = min(canvas.height, int(math.ceil(max(ys))) + 2)
        width = x1 - x0
        height = y1 - y0
        if width <= 0 or height <= 0:
            raise SystemExit(f"text falls outside the image: {text!r}")
        sw, sh = width * samples, height * samples
        coverage = bytearray(sw * sh)
        # All contours of a glyph share one fill, so an inner contour
        # knocks a counter out of the outer one instead of painting over it.
        segments: list[tuple[float, float, float, float, int]] = []
        for poly in polygons:
            if len(poly) < 3:
                continue
            count = len(poly)
            for index in range(count):
                x_a, y_a = poly[index]
                x_b, y_b = poly[(index + 1) % count]
                if y_a == y_b:
                    continue
                if y_a < y_b:
                    segments.append((y_a, y_b, x_a, x_b, 1))
                else:
                    segments.append((y_b, y_a, x_b, x_a, -1))
        if segments:
            scaled = [
                ((y0s - y0) * samples, (y1s - y0) * samples, (x0s - x0) * samples, (x1s - x0) * samples, direction)
                for y0s, y1s, x0s, x1s, direction in segments
            ]
            top = max(0, int(min(seg[0] for seg in scaled)) - 1)
            bottom = min(sh - 1, int(max(seg[1] for seg in scaled)) + 1)
            for yy in range(top, bottom + 1):
                scan = yy + 0.5
                hits: list[tuple[float, int]] = []
                for y_start, y_end, x_start, x_end, direction in scaled:
                    if y_start <= scan < y_end:
                        t = (scan - y_start) / (y_end - y_start)
                        hits.append((x_start + t * (x_end - x_start), direction))
                hits.sort(key=lambda item: item[0])
                winding = 0
                previous: float | None = None
                row = yy * sw
                for hit_x, direction in hits:
                    if previous is not None and winding != 0:
                        left = max(0, math.ceil(previous - 0.5))
                        right = min(sw, math.floor(hit_x - 0.5) + 1)
                        if right > left:
                            coverage[row + left : row + right] = b"\x01" * (right - left)
                    winding += direction
                    previous = hit_x
        area = samples * samples
        for y in range(height):
            for x in range(width):
                total = 0
                for sy in range(samples):
                    row = (y * samples + sy) * sw + x * samples
                    total += sum(coverage[row : row + samples])
                if total:
                    canvas.blend(x0 + x, y0 + y, INK, (total * 255 + area // 2) // area)

    return text_width, draw_text


def _open_counter_pixels(canvas: Canvas) -> int:
    """Paper pixels enclosed by ink. A closed bowl has none."""
    width, height = canvas.width, canvas.height
    seen = bytearray(width * height)
    stack: list[tuple[int, int]] = []
    for x in range(width):
        stack.append((x, 0))
        stack.append((x, height - 1))
    for y in range(height):
        stack.append((0, y))
        stack.append((width - 1, y))
    while stack:
        x, y = stack.pop()
        if not (0 <= x < width and 0 <= y < height):
            continue
        pixel = y * width + x
        if seen[pixel]:
            continue
        if canvas.buf[pixel * 4] < 180:
            continue
        seen[pixel] = 1
        stack.append((x + 1, y))
        stack.append((x - 1, y))
        stack.append((x, y + 1))
        stack.append((x, y - 1))
    enclosed = 0
    for pixel in range(width * height):
        if canvas.buf[pixel * 4] >= 180 and not seen[pixel]:
            enclosed += 1
    return enclosed


def assert_open_counters(draw_text) -> None:
    """At feed size, the bowls that faux bold filled in must stay open."""
    letters = "Oeaogdb"
    # Card sizes scaled to a 360px-wide preview of the 1200px frame.
    sizes = [round(size * 360 / 1200) for size in (164, 148, 86, 76)]
    for size in sizes:
        for char in letters:
            canvas = Canvas(size * 3, size * 3, PAPER, alpha=255)
            draw_text(canvas, char, size * 0.4, size * 1.6, size, 0)
            hole = _open_counter_pixels(canvas)
            if hole < max(6, size * size // 80):
                raise SystemExit(f"{char!r} at {size}px has a closed counter ({hole}px)")


def favicon_svg() -> str:
    return (
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16">\n'
        "<title>Another</title>\n"
        '<rect width="16" height="16" rx="2" fill="#f8f8f4"/>\n'
        '<rect x="2" y="3" width="3" height="10" fill="#292922"/>\n'
        '<rect x="11" y="3" width="3" height="10" fill="#292922"/>\n'
        '<rect x="6" y="9" width="4" height="4" fill="#705000"/>\n'
        "</svg>\n"
    )


def manifest() -> str:
    data = {
        "name": "Another",
        "short_name": "Another",
        "description": "Local coordination for coding agents: messages, handoffs, and path holds.",
        "icons": [
            {"src": "/icon-192.png", "sizes": "192x192", "type": "image/png"},
            {"src": "/icon-512.png", "sizes": "512x512", "type": "image/png"},
        ],
        "background_color": "#f8f8f4",
        "theme_color": "#f8f8f4",
        "display": "browser",
    }
    return json.dumps(data, indent=2) + "\n"


def icon_image(size: int, tile: bool) -> Canvas:
    # Snap to the 16px grid so the bars stay hard-edged. 180 is not a multiple
    # of 16, so that tile is the 176px mark centered on opaque paper.
    draw = (size // TILE) * TILE
    origin = (size - draw) // 2
    if tile:
        canvas = Canvas(size, size, PAPER, alpha=0)
    else:
        canvas = Canvas(size, size, PAPER, alpha=255)
    paint_mark(canvas, origin, origin, draw, tile=tile)
    return canvas


def assert_favicon_pixels(canvas: Canvas) -> None:
    if canvas.width != 16:
        return

    def pixel(x: int, y: int) -> tuple[int, int, int, int]:
        i = (y * canvas.width + x) * 4
        buf = canvas.buf
        return (buf[i], buf[i + 1], buf[i + 2], buf[i + 3])

    # Corner of the rounded tile is outside the shape.
    corner = pixel(0, 0)
    # The outer corner is antialiased, so the corner pixel is paper with low alpha.
    if corner[3] > 80 or corner[:3] != PAPER:
        raise SystemExit(f"favicon corner should be mostly transparent paper, got {corner}")
    # Agents, the gap, and the message.
    ink = pixel(3, 5)
    gap = pixel(5, 10)
    message = pixel(7, 11)
    if ink[:3] != INK or gap[:3] != PAPER or message[:3] != OCHRE:
        raise SystemExit(f"favicon pixels drifted: ink {ink} gap {gap} message {message}")




def _line_height(size: float) -> float:
    return (CAP + DESC) * size


def _draw_line(draw_text, canvas: Canvas, text: str, x: float, top: float, size: float, tracking: float) -> float:
    draw_text(canvas, text, x, top + CAP * size, size, tracking)
    return top + _line_height(size)


def build_card(text_width, draw_text, kind: str) -> Canvas:
    """1200x630 cards. Twitter center-crops this frame to 2:1 (1200x600).

    Every mark and letter sits inside that band, and inside a wider margin,
    so a center crop does not cut a word. The headline is at least half the
    frame wide. Text is ink on paper, set in JetBrains Mono ExtraBold.
    """
    width, height = 1200, 630
    canvas = Canvas(width, height, PAPER, alpha=255)
    # Visible band after a centered 2:1 crop, plus a pad so strokes are not on the cut.
    crop_top = (height - width // 2) // 2
    crop_bottom = height - crop_top
    if kind == "og":
        mark = 112
        word_size, headline_size, detail_size = 52, 164, 86
        gap_mark, gap_word, gap_head = 18, 36, 20
        headline_w = text_width(HEADLINE, headline_size, 0)
        detail_w = text_width(DETAIL, detail_size, 0)
        if headline_w < width * 0.5:
            raise SystemExit(f"headline is only {headline_w:.0f}px wide")
        block = (
            mark + gap_mark + _line_height(word_size) + gap_word
            + _line_height(headline_size) + gap_head + _line_height(detail_size)
        )
        top = (height - block) / 2
        grid_x = (width - mark) / 2
        paint_mark(canvas, grid_x, top, mark, tile=False)
        y = top + mark + gap_mark
        word_w = text_width(WORDMARK, word_size, -0.03)
        y = _draw_line(draw_text, canvas, WORDMARK, (width - word_w) / 2, y, word_size, -0.03)
        y += gap_word
        y = _draw_line(draw_text, canvas, HEADLINE, (width - headline_w) / 2, y, headline_size, 0)
        y += gap_head
        _draw_line(draw_text, canvas, DETAIL, (width - detail_w) / 2, y, detail_size, 0)
    elif kind == "twitter":
        # Same words, mark beside them, the whole lockup centered in the frame.
        mark = 128
        word_size, headline_size, detail_size = 48, 148, 76
        gap_word, gap_head = 22, 16
        gap_mark = 40
        headline_w = text_width(HEADLINE, headline_size, 0)
        detail_w = text_width(DETAIL, detail_size, 0)
        word_w = text_width(WORDMARK, word_size, -0.03)
        if headline_w < width * 0.5:
            raise SystemExit(f"headline is only {headline_w:.0f}px wide")
        text_w = max(word_w, headline_w, detail_w)
        bars_w = (12 / TILE) * mark
        block_w = bars_w + gap_mark + text_w
        block_h = _line_height(word_size) + gap_word + _line_height(headline_size) + gap_head + _line_height(detail_size)
        left = (width - block_w) / 2
        top = (height - block_h) / 2
        grid_x = left - (2 / TILE) * mark
        grid_y = top + (block_h - mark) / 2
        paint_mark(canvas, grid_x, grid_y, mark, tile=False)
        text_x = left + bars_w + gap_mark
        y = top
        y = _draw_line(draw_text, canvas, WORDMARK, text_x, y, word_size, -0.03)
        y += gap_word
        y = _draw_line(draw_text, canvas, HEADLINE, text_x, y, headline_size, 0)
        y += gap_head
        _draw_line(draw_text, canvas, DETAIL, text_x, y, detail_size, 0)
    else:
        raise SystemExit(f"unknown card {kind}")
    _assert_crop_safe(canvas, crop_top + 8, crop_bottom - 8)
    return canvas


def _assert_crop_safe(canvas: Canvas, top: int, bottom: int) -> None:
    """Non-paper pixels must survive a centered 2:1 crop and stay off the side edges."""
    side = 36
    paper = bytes(PAPER)
    for y in range(canvas.height):
        row = y * canvas.width * 4
        for x in range(canvas.width):
            i = row + x * 4
            if canvas.buf[i : i + 3] == paper:
                continue
            if y < top or y >= bottom or x < side or x >= canvas.width - side:
                raise SystemExit(f"mark or text would be cropped at {x},{y}")


def nearest(canvas: Canvas, scale: int) -> Canvas:
    out = Canvas(canvas.width * scale, canvas.height * scale, PAPER, alpha=0)
    for y in range(canvas.height):
        for x in range(canvas.width):
            i = (y * canvas.width + x) * 4
            pixel = bytes(canvas.buf[i : i + 4])
            for sy in range(scale):
                row = ((y * scale + sy) * out.width + x * scale) * 4
                for sx in range(scale):
                    out.buf[row + sx * 4 : row + sx * 4 + 4] = pixel
    return out


def fit_width(canvas: Canvas, width: int) -> Canvas:
    height = round(canvas.height * width / canvas.width)
    out = Canvas(width, height, PAPER, alpha=255)
    for y in range(height):
        sy = min(canvas.height - 1, int(y * canvas.height / height))
        for x in range(width):
            sx = min(canvas.width - 1, int(x * canvas.width / width))
            i = (sy * canvas.width + sx) * 4
            j = (y * width + x) * 4
            out.buf[j : j + 4] = canvas.buf[i : i + 4]
    return out


def _blit(target: Canvas, image: Canvas, origin_x: int, origin_y: int) -> None:
    for y in range(image.height):
        dy = origin_y + y
        if not (0 <= dy < target.height):
            continue
        for x in range(image.width):
            dx = origin_x + x
            if not (0 <= dx < target.width):
                continue
            i = (y * image.width + x) * 4
            alpha = image.buf[i + 3]
            j = (dy * target.width + dx) * 4
            if alpha >= 255:
                target.buf[j : j + 4] = image.buf[i : i + 4]
            elif alpha > 0:
                scale = alpha / 255
                for channel in range(3):
                    target.buf[j + channel] = int(image.buf[i + channel] * scale + target.buf[j + channel] * (1 - scale) + 0.5)
                target.buf[j + 3] = 255


def _on_backdrop(image: Canvas, backdrop: tuple[int, int, int], pad: int) -> Canvas:
    frame = Canvas(image.width + pad * 2, image.height + pad * 2, backdrop, alpha=255)
    _blit(frame, image, pad, pad)
    return frame


def write_previews(directory: Path, draw_text, icons: dict[str, Canvas], og: Canvas, twitter: Canvas) -> None:
    directory.mkdir(parents=True, exist_ok=True)
    # Enlarged with nearest-neighbor so the 16 and 32 pixel grids stay visible.
    shown = (
        ("16px, shown 8 times larger", nearest(icons["16"], 8), icons["16"]),
        ("32px, shown 4 times larger", nearest(icons["32"], 4), icons["32"]),
        ("180px", icons["180"], None),
    )
    pad = 32
    columns = []
    for label, enlarged, actual in shown:
        light = _on_backdrop(enlarged, (0xE4, 0xE2, 0xD8), 24)
        dark = _on_backdrop(enlarged, (0x2B, 0x2B, 0x28), 24)
        row = Canvas(light.width + dark.width + 16, light.height, (255, 255, 255), alpha=255)
        _blit(row, light, 0, 0)
        _blit(row, dark, light.width + 16, 0)
        columns.append((label, row, actual))
    label_h = 36
    col_w = max(row.width for _label, row, _actual in columns)
    sheet_w = pad + (col_w + pad) * len(columns)
    sheet_h = pad + label_h + max(row.height for _label, row, _actual in columns) + 72
    sheet = Canvas(sheet_w, sheet_h, (255, 255, 255), alpha=255)
    x = pad
    for label, row, actual in columns:
        draw_text(sheet, label, x, pad + 22, 18, -0.02, samples=2)
        _blit(sheet, row, x, pad + label_h)
        if actual is not None:
            chip = _on_backdrop(actual, (0xE4, 0xE2, 0xD8), 8)
            _blit(sheet, chip, x, pad + label_h + row.height + 12)
        x += col_w + pad
    write_png(directory / "favicon-previews.png", sheet.width, sheet.height, sheet.bytes())
    write_png(directory / "og.png", og.width, og.height, og.bytes())
    write_png(directory / "twitter.png", twitter.width, twitter.height, twitter.bytes())
    for name, image in (("og", og), ("twitter", twitter)):
        for thumb_w in (360, 480):
            thumb = fit_width(image, thumb_w)
            write_png(directory / f"{name}-{thumb_w}.png", thumb.width, thumb.height, thumb.bytes())


def main() -> None:
    parser = argparse.ArgumentParser(description="Regenerate Another site icons and social images.")
    parser.add_argument("--out", type=Path, default=ROOT)
    parser.add_argument("--previews", type=Path)
    args = parser.parse_args()
    out: Path = args.out
    out.mkdir(parents=True, exist_ok=True)

    text_width, draw_text = load_font()
    assert_open_counters(draw_text)
    (out / "favicon.svg").write_text(favicon_svg(), encoding="utf-8")
    (out / "site.webmanifest").write_text(manifest(), encoding="utf-8")

    icons = {
        "16": icon_image(16, tile=True),
        "32": icon_image(32, tile=True),
        "48": icon_image(48, tile=True),
        "180": icon_image(180, tile=False),
        "192": icon_image(192, tile=False),
        "512": icon_image(512, tile=False),
    }
    assert_favicon_pixels(icons["16"])
    for name, size in (("apple-touch-icon.png", "180"), ("icon-192.png", "192"), ("icon-512.png", "512")):
        image = icons[size]
        write_png(out / name, image.width, image.height, image.bytes())
    ico_images = []
    for size in (16, 32, 48):
        image = icons[str(size)]
        ico_images.append((size, size, png_bytes(image.width, image.height, image.bytes())))
    write_ico(out / "favicon.ico", ico_images)

    og = build_card(text_width, draw_text, "og")
    twitter = build_card(text_width, draw_text, "twitter")
    if og.bytes() == twitter.bytes():
        raise SystemExit("og.png and twitter.png came out identical")
    write_png(out / "og.png", og.width, og.height, og.bytes())
    write_png(out / "twitter.png", twitter.width, twitter.height, twitter.bytes())
    if args.previews:
        write_previews(args.previews, draw_text, icons, og, twitter)
    print(f"wrote icons and social images to {out}")


if __name__ == "__main__":
    main()
