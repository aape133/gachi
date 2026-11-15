"""Draws the gachi logo (two overlapping circles) as SVG and PNG, with no image libraries.

    python branding/make_logo.py

Writes, next to this file:
  gachi-logo.svg / gachi-logo-1024.png   the mark alone, transparent background
  gachi-icon.svg / gachi-icon-1024.png   the app icon: the mark on a dark rounded tile
The app icons (app/icons) come from gachi-icon-1024.png via `npx tauri icon`.
"""
import math
import os
import struct
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
PURPLE = (139, 124, 255)
GREEN = (61, 220, 151)
TILE = (12, 13, 16)
# the two circles overlap like light ("screen" blend): where both are, it brightens
OVERLAP = tuple(255 - (255 - a) * (255 - b) // 255 for a, b in zip(PURPLE, GREEN))

# the in-app mark: viewBox 30×20, circles r=8 at x=10 and x=20
R, CX1, CX2, CY = 8, 10, 20, 10


def svg(tile: bool) -> str:
    hexc = lambda c: '#%02x%02x%02x' % c
    if tile:
        # 100×100 tile, mark scaled to 70% of the width and centered
        s = 70 / 30
        ox, oy = 15, 50 - CY * s
        body = (f'<rect width="100" height="100" rx="22" fill="{hexc(TILE)}"/>'
                f'<g transform="translate({ox:.3f} {oy:.3f}) scale({s:.4f})">')
        end = '</g>'
        vb = '0 0 100 100'
    else:
        body, end, vb = '', '', '0 0 30 20'
    return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{vb}">{body}'
            f'<circle cx="{CX1}" cy="{CY}" r="{R}" fill="{hexc(PURPLE)}"/>'
            f'<circle cx="{CX2}" cy="{CY}" r="{R}" fill="{hexc(GREEN)}" style="mix-blend-mode:screen"/>'
            f'<path d="M15 3.76 A8 8 0 0 1 15 16.24 A8 8 0 0 1 15 3.76Z" fill="{hexc(OVERLAP)}"/>'
            f'{end}</svg>\n')


def png(path: str, size: int, tile: bool) -> None:
    # coordinates in mark units
    scale = size * (0.70 if tile else 0.94) / 30
    ox = (size - 30 * scale) / 2
    oy = size / 2 - CY * scale
    corner = size * 0.22
    rows = []
    for y in range(size):
        row = bytearray([0])  # PNG filter: none
        for x in range(size):
            px, py = x + 0.5, y + 0.5
            mx, my = (px - ox) / scale, (py - oy) / scale
            # anti-aliased coverage from the signed distance (in pixels)
            d1 = (math.hypot(mx - CX1, my - CY) - R) * scale
            d2 = (math.hypot(mx - CX2, my - CY) - R) * scale
            a1 = min(1.0, max(0.0, 0.5 - d1))
            a2 = min(1.0, max(0.0, 0.5 - d2))
            # purple only, green only, both (overlap) — weights add up to the mark's coverage
            wp, wg, wo = a1 * (1 - a2), a2 * (1 - a1), a1 * a2
            am = wp + wg + wo
            mark = [(wp * p + wg * g + wo * o) / am if am else 0 for p, g, o in zip(PURPLE, GREEN, OVERLAP)]
            if tile:
                # rounded-square tile under the mark
                qx = max(abs(px - size / 2) - (size / 2 - corner), 0)
                qy = max(abs(py - size / 2) - (size / 2 - corner), 0)
                at = min(1.0, max(0.0, 0.5 - (math.hypot(qx, qy) - corner)))
                col = [t * (1 - am) + m * am for t, m in zip(TILE, mark)]
                a = at
            else:
                col, a = mark, am
            row += bytes([max(0, min(255, round(c))) for c in col] + [round(a * 255)])
        rows.append(bytes(row))
    raw = zlib.compress(b''.join(rows), 9)

    def chunk(t, data):
        return struct.pack('>I', len(data)) + t + data + struct.pack('>I', zlib.crc32(t + data) & 0xFFFFFFFF)

    out = b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', size, size, 8, 6, 0, 0, 0))
    out += chunk(b'IDAT', raw) + chunk(b'IEND', b'')
    with open(path, 'wb') as f:
        f.write(out)


if __name__ == '__main__':
    for name, tile in (('gachi-logo', False), ('gachi-icon', True)):
        with open(os.path.join(HERE, name + '.svg'), 'w', encoding='utf-8') as f:
            f.write(svg(tile))
        png(os.path.join(HERE, name + '-1024.png'), 1024, tile)
        print('wrote', name)
