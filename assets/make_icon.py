"""Draws assets/plate.ico: an orange cube sitting on a print bed.

No dependencies: it rasterizes the shapes itself (4x4 supersampling per
pixel) at each icon size and packs PNG images into the .ico. Run it from
anywhere with `python assets/make_icon.py` after changing the design.
"""

import math
import struct
import zlib
from pathlib import Path

SIZES = [16, 24, 32, 48, 64, 128, 256]
SUPERSAMPLE = 4

# Colors (sRGB). The bed matches the viewer's background and plate outline.
BED_TOP = (52, 58, 68)
BED_LEFT = (36, 40, 48)
BED_RIGHT = (28, 31, 38)
BED_LINE = (90, 150, 210)
CUBE_TOP = (255, 190, 105)
CUBE_LEFT = (235, 145, 60)
CUBE_RIGHT = (196, 108, 38)

BED_HALF = 1.0  # bed spans -1..1 in x and y
BED_THICKNESS = 0.16
CUBE_HALF = 0.42


def project(x, y, z):
    """Isometric view from the +x, +y side, z up; image y grows downward."""
    return ((x - y) * math.cos(math.radians(30)), (x + y) * math.sin(math.radians(30)) - z)


def box_faces(half, z0, z1, top, left, right):
    """The three visible faces of an axis-aligned box, back to front."""
    h = half
    return [
        (left, [project(-h, h, z0), project(h, h, z0), project(h, h, z1), project(-h, h, z1)]),
        (right, [project(h, -h, z0), project(h, h, z0), project(h, h, z1), project(h, -h, z1)]),
        (top, [project(-h, -h, z1), project(h, -h, z1), project(h, h, z1), project(-h, h, z1)]),
    ]


def line(a, b, width):
    """A segment as a thin quad."""
    (ax, ay), (bx, by) = a, b
    length = math.hypot(bx - ax, by - ay)
    nx, ny = -(by - ay) / length * width / 2, (bx - ax) / length * width / 2
    return [(ax + nx, ay + ny), (bx + nx, by + ny), (bx - nx, by - ny), (ax - nx, ay - ny)]


def scene(size):
    """Polygons in painting order: (rgb, [(u, v), ...])."""
    polygons = box_faces(BED_HALF, -BED_THICKNESS, 0.0, BED_TOP, BED_LEFT, BED_RIGHT)
    # Plate outline, a little wider at small sizes so it survives.
    width = max(0.05, 1.6 / size)
    h = BED_HALF * 0.9
    corners = [project(-h, -h, 0), project(h, -h, 0), project(h, h, 0), project(-h, h, 0)]
    for i in range(4):
        polygons.append((BED_LINE, line(corners[i], corners[(i + 1) % 4], width)))
    polygons += box_faces(CUBE_HALF, 0.0, 2 * CUBE_HALF, CUBE_TOP, CUBE_LEFT, CUBE_RIGHT)
    return polygons


def rasterize(size):
    polygons = scene(size)
    points = [p for _, poly in polygons for p in poly]
    min_u = min(p[0] for p in points)
    max_u = max(p[0] for p in points)
    min_v = min(p[1] for p in points)
    max_v = max(p[1] for p in points)
    margin = 0.04 * size
    scale = (size - 2 * margin) / max(max_u - min_u, max_v - min_v)
    offset_u = (size - (max_u - min_u) * scale) / 2 - min_u * scale
    offset_v = (size - (max_v - min_v) * scale) / 2 - min_v * scale

    n = size * SUPERSAMPLE
    palette = [None] + [rgb for rgb, _ in polygons]
    samples = bytearray(n * n)  # palette index per subsample; 0 = transparent
    for index, (_, poly) in enumerate(polygons, start=1):
        pts = [((u * scale + offset_u) * SUPERSAMPLE, (v * scale + offset_v) * SUPERSAMPLE) for u, v in poly]
        # Orient counter-clockwise (in image coordinates) so inside is edge >= 0.
        area = sum(pts[i][0] * pts[(i + 1) % len(pts)][1] - pts[(i + 1) % len(pts)][0] * pts[i][1] for i in range(len(pts)))
        if area < 0:
            pts.reverse()
        edges = [(pts[i], pts[(i + 1) % len(pts)]) for i in range(len(pts))]
        x0 = max(0, int(min(p[0] for p in pts)))
        x1 = min(n - 1, int(max(p[0] for p in pts)) + 1)
        y0 = max(0, int(min(p[1] for p in pts)))
        y1 = min(n - 1, int(max(p[1] for p in pts)) + 1)
        for y in range(y0, y1 + 1):
            py = y + 0.5
            row = y * n
            for x in range(x0, x1 + 1):
                px = x + 0.5
                if all((bx - ax) * (py - ay) - (by - ay) * (px - ax) >= 0 for (ax, ay), (bx, by) in edges):
                    samples[row + x] = index

    pixels = bytearray()
    for y in range(size):
        pixels.append(0)  # PNG filter: none
        for x in range(size):
            r = g = b = covered = 0
            for sy in range(SUPERSAMPLE):
                row = (y * SUPERSAMPLE + sy) * n + x * SUPERSAMPLE
                for sx in range(SUPERSAMPLE):
                    index = samples[row + sx]
                    if index:
                        cr, cg, cb = palette[index]
                        r, g, b, covered = r + cr, g + cg, b + cb, covered + 1
            if covered:
                pixels += bytes((r // covered, g // covered, b // covered, covered * 255 // SUPERSAMPLE**2))
            else:
                pixels += b"\0\0\0\0"
    return png(size, bytes(pixels))


def png(size, raw):
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)  # 8-bit RGBA
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b"")


def ico(images):
    """An .ico whose entries are PNGs (supported since Windows Vista)."""
    out = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    data = b""
    for size, image in images:
        dim = 0 if size >= 256 else size
        out += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(image), offset + len(data))
        data += image
    return out + data


if __name__ == "__main__":
    here = Path(__file__).resolve().parent
    images = [(size, rasterize(size)) for size in SIZES]
    (here / "plate.ico").write_bytes(ico(images))
    (here / "plate.png").write_bytes(dict(images)[256])
    print(f"wrote {here / 'plate.ico'} ({', '.join(str(s) for s in SIZES)} px) and plate.png")
