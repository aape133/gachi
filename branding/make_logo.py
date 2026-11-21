"""Draws the gachi logo (five bars of a waveform, in two tones) as SVG, PNG and ICO, with no
image libraries.

    python branding/make_logo.py

Writes, next to this file:
  gachi-logo.svg / gachi-logo-1024.png   the bars alone, black on a transparent background
  gachi-icon.svg / gachi-icon-1024.png   the app icon: white bars on a black rounded tile
and the app's own icons in app/icons (32, 128, 256, 512 PNG and icon.ico with 16-256).

The bars are the in-app logo's (ui/index.html): viewBox 24x24, stroke 2.4, round caps, on a
tile 22/15 the size of that box with a 6/22 corner. The smallest icons (16, 24, 32) have their
bars put on whole pixels so they stay sharp.
"""
import math
import os
import struct
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
ICONS = os.path.join(HERE, '..', 'app', 'icons')
TILE = (17, 18, 21)     # the light theme's filled tile, near black
INK = (245, 246, 248)   # the bars on it, near white
BLACK = (17, 18, 21)

# the in-app mark, in its 24-unit box: (x, top, bottom) of each bar
BARS = [(4, 10, 14), (8, 7, 17), (12, 4.5, 19.5), (16, 7, 17), (20, 10, 14)]
STROKE = 2.4
GLYPH = 15 / 22          # the box's share of the tile
CORNER = 6 / 22          # the tile's corner, as a share of its side


def svg(tile: bool) -> str:
    d = ''.join(f'M{x} {t}v{b - t:g}' for x, t, b in BARS)
    if tile:
        side = 24 / GLYPH                       # the tile in box units
        o = (side - 24) / 2
        return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{-o:.2f} {-o:.2f} {side:.2f} {side:.2f}">'
                f'<rect x="{-o:.2f}" y="{-o:.2f}" width="{side:.2f}" height="{side:.2f}" rx="{side * CORNER:.2f}" fill="#{"%02x%02x%02x" % TILE}"/>'
                f'<path d="{d}" fill="none" stroke="#{"%02x%02x%02x" % INK}" stroke-width="{STROKE}" stroke-linecap="round"/></svg>\n')
    return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="2 3.3 20 17.4">'
            f'<path d="{d}" fill="none" stroke="#{"%02x%02x%02x" % BLACK}" stroke-width="{STROKE}" stroke-linecap="round"/></svg>\n')


def bars_px(size: int, tile: bool):
    """Each bar as (x, top, bottom, radius) in pixels."""
    if tile:
        u = size * GLYPH / 24
        ox = oy = size * (1 - GLYPH) / 2
    else:
        # the bars alone fill the picture: 20 x 17.4 units (with the caps), centered
        u = size * 0.94 / 20
        ox = (size - 20 * u) / 2 - 2 * u
        oy = (size - 17.4 * u) / 2 - 3.3 * u
    if tile and size <= 32:
        # whole pixels: bar width w, gap g, the five centered; heights rounded to even spans
        w = max(1, round(STROKE * u))
        g = max(1, round(4 * u - w))
        span = 5 * w + 4 * g
        left = (size - span) // 2
        out = []
        for i, (_, t, b) in enumerate(BARS):
            x0 = left + i * (w + g)
            h = max(w + 1, round((b - t + STROKE) * u))
            if (size - h) % 2:
                h += 1
            top = (size - h) / 2
            r = w / 2
            out.append((x0 + r, top + r, top + h - r, r))
        return out
    return [(ox + x * u, oy + t * u, oy + b * u, STROKE / 2 * u) for x, t, b in BARS]


def png_bytes(size: int, tile: bool) -> bytes:
    bars = bars_px(size, tile)
    corner = size * CORNER
    bg, fg = (TILE, INK) if tile else (None, BLACK)
    rows = []
    for y in range(size):
        row = bytearray([0])  # PNG filter: none
        py = y + 0.5
        for x in range(size):
            px = x + 0.5
            # anti-aliased coverage from the signed distance (in pixels) to the nearest bar
            d = min(math.hypot(px - bx, py - min(max(py, top), bot)) - r for bx, top, bot, r in bars)
            ab = min(1.0, max(0.0, 0.5 - d))
            if tile:
                qx = max(abs(px - size / 2) - (size / 2 - corner), 0)
                qy = max(abs(py - size / 2) - (size / 2 - corner), 0)
                at = min(1.0, max(0.0, 0.5 - (math.hypot(qx, qy) - corner)))
                col = [t * (1 - ab) + f * ab for t, f in zip(bg, fg)]
                a = at
            else:
                col, a = fg, ab
            row += bytes([max(0, min(255, round(c))) for c in col] + [round(a * 255)])
        rows.append(bytes(row))

    def chunk(t, data):
        return struct.pack('>I', len(data)) + t + data + struct.pack('>I', zlib.crc32(t + data) & 0xFFFFFFFF)

    out = b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', size, size, 8, 6, 0, 0, 0))
    return out + chunk(b'IDAT', zlib.compress(b''.join(rows), 9)) + chunk(b'IEND', b'')


def ico_bytes(sizes) -> bytes:
    """An .ico holding one PNG per size (Windows Vista and later read these)."""
    pngs = [png_bytes(s, True) for s in sizes]
    head = struct.pack('<HHH', 0, 1, len(sizes))
    offset = 6 + 16 * len(sizes)
    entries = b''
    for s, p in zip(sizes, pngs):
        entries += struct.pack('<BBBBHHII', s % 256, s % 256, 0, 0, 1, 32, len(p), offset)
        offset += len(p)
    return head + entries + b''.join(pngs)


def write(path: str, data: bytes) -> None:
    with open(path, 'wb') as f:
        f.write(data)
    print('wrote', os.path.relpath(path, os.path.join(HERE, '..')))


if __name__ == '__main__':
    for name, tile in (('gachi-logo', False), ('gachi-icon', True)):
        write(os.path.join(HERE, name + '.svg'), svg(tile).encode())
        write(os.path.join(HERE, name + '-1024.png'), png_bytes(1024, tile))
    for file, size in (('32x32.png', 32), ('128x128.png', 128), ('128x128@2x.png', 256), ('icon.png', 512)):
        write(os.path.join(ICONS, file), png_bytes(size, True))
    write(os.path.join(ICONS, 'icon.ico'), ico_bytes([16, 24, 32, 48, 64, 256]))
