"""Draw YSpot's own icons on the Fluent 20px grid.

Two apps have no icon of their own: Windows Security and Windows Backup both
fall back to the Windows Settings glyph, so three rows in the launcher look
identical. Fluent's MIT set has no icon for either, and Microsoft's shipped
artwork is not ours to copy. So these are drawn.

Every number here comes from docs/icon-grid.md, which was read out of the
Fluent iconography community file itself: a 20 box, a 2 margin, a 16 live area,
1px Regular stroke, 1.5px minimum feature, and the keylines a shape must fill
to look the same visual size as its neighbours. Drawing an original shape to a
published grid is what a published grid is for. Nothing is copied from that
file -- these drawings are YSpot's, under YSpot's licence.

The output is a single filled path per icon, wound so that holes are holes
under the default nonzero fill rule: no fill-rule attribute, no strokes,
because that is exactly what the renderer applies and what
vendor-fluent-icons.py asserts about every other glyph in the set.

Usage:
    python scripts/draw-yspot-icons.py                   check and report
    python scripts/draw-yspot-icons.py --emit            write the TS module
    python scripts/draw-yspot-icons.py --preview p.html  a sheet to look at
"""

import io
import math
import sys

OUT = "apps/shell/src/lib/yspotGlyphs.ts"

# --- path building -------------------------------------------------------
#
# A path is a list of segments. A segment is (p0, c1, c2, p3) for a cubic, or
# (p0, None, None, p3) for a line. Keeping that shape means a path can be
# reversed exactly, by swapping the handles and reversing the order, which is
# how a hole gets the opposite winding from the shape it sits in. Reversing
# beats authoring each shape twice: the two copies cannot drift apart.

KAPPA = 0.5522847498307936


def line(p0, p3):
    return (p0, None, None, p3)


def reverse(segs):
    return [(s[3], s[2], s[1], s[0]) for s in reversed(segs)]


def polar(cx, cy, r, a):
    b = math.radians(a)
    return (cx + r * math.cos(b), cy + r * math.sin(b))


def rounded_rect(x, y, w, h, r):
    """Clockwise in screen coordinates, starting at the top-left tangent."""
    k = r * KAPPA
    x1, y1 = x + w, y + h
    return [
        ((x + r, y), None, None, (x1 - r, y)),
        ((x1 - r, y), (x1 - r + k, y), (x1, y + r - k), (x1, y + r)),
        ((x1, y + r), None, None, (x1, y1 - r)),
        ((x1, y1 - r), (x1, y1 - r + k), (x1 - r + k, y1), (x1 - r, y1)),
        ((x1 - r, y1), None, None, (x + r, y1)),
        ((x + r, y1), (x + r - k, y1), (x, y1 - r + k), (x, y1 - r)),
        ((x, y1 - r), None, None, (x, y + r)),
        ((x, y + r), (x, y + r - k), (x + r - k, y), (x + r, y)),
    ]


def arc(cx, cy, r, a0, a1):
    """Circular arc as cubics, angles in degrees, screen coordinates.

    Increasing angle is clockwise on screen, because y grows downward.
    """
    segs = []
    steps = max(1, int(math.ceil(abs(a1 - a0) / 90.0)))
    step = (a1 - a0) / steps
    h = 4.0 / 3.0 * math.tan(math.radians(step) / 4.0)
    for i in range(steps):
        b0 = math.radians(a0 + step * i)
        b1 = math.radians(a0 + step * (i + 1))
        p0 = (cx + r * math.cos(b0), cy + r * math.sin(b0))
        p3 = (cx + r * math.cos(b1), cy + r * math.sin(b1))
        c1 = (p0[0] - h * r * math.sin(b0), p0[1] + h * r * math.cos(b0))
        c2 = (p3[0] + h * r * math.sin(b1), p3[1] - h * r * math.cos(b1))
        segs.append((p0, c1, c2, p3))
    return segs


def corner(p0, p3, centre):
    """A quarter circle from p0 to p3 about centre, as one cubic.

    Handles either turn direction, so the same call draws an outside corner
    and the inside corner of an L.
    """
    r = math.hypot(p0[0] - centre[0], p0[1] - centre[1])
    a0 = math.atan2(p0[1] - centre[1], p0[0] - centre[0])
    a1 = math.atan2(p3[1] - centre[1], p3[0] - centre[0])
    while a1 - a0 > math.pi:
        a1 -= 2 * math.pi
    while a0 - a1 > math.pi:
        a1 += 2 * math.pi
    h = 4.0 / 3.0 * math.tan((a1 - a0) / 4.0)
    c1 = (p0[0] - h * r * math.sin(a0), p0[1] + h * r * math.cos(a0))
    c2 = (p3[0] + h * r * math.sin(a1), p3[1] - h * r * math.cos(a1))
    return (p0, c1, c2, p3)


def clockwise_arrow(cx, cy, rc, w, gap_from, gap_to, head_half, head_len):
    """An open ring with an arrowhead at its clockwise end.

    Material runs clockwise from gap_to round to gap_from. The head is a
    triangle spanning head_half either side of the ring centreline, so it is
    the widest point: keep cx +/- (rc + head_half) inside whatever contains it.
    """
    ro, ri = rc + w / 2.0, rc - w / 2.0
    end = gap_from + 360.0
    segs = arc(cx, cy, ro, gap_to, end)
    base = polar(cx, cy, rc, end)
    a = math.radians(end)
    tip = (base[0] - math.sin(a) * head_len, base[1] + math.cos(a) * head_len)
    outer_base = polar(cx, cy, rc + head_half, end)
    inner_base = polar(cx, cy, rc - head_half, end)
    segs.append(line(segs[-1][3], outer_base))
    segs.append(line(outer_base, tip))
    segs.append(line(tip, inner_base))
    segs.append(line(inner_base, polar(cx, cy, ri, end)))
    segs += arc(cx, cy, ri, end, gap_to)
    segs.append(line(segs[-1][3], segs[0][0]))
    return segs


def bracket(x0, y0, x1, y1, t, r_out, r_in, r_cap):
    """The top and right edges of a plate, as an L of thickness t.

    A sheet showing from behind another one. Drawn as its own shape because
    the plate-minus-plate that would produce it is a boolean subtraction, and
    a boolean is the one thing a single filled path cannot express -- see the
    note above SHAPES.
    """
    ix, iy = x1 - t, y0 + t
    return [
        line((x0 + r_cap, y0), (x1 - r_out, y0)),
        corner((x1 - r_out, y0), (x1, y0 + r_out), (x1 - r_out, y0 + r_out)),
        line((x1, y0 + r_out), (x1, y1 - r_cap)),
        corner((x1, y1 - r_cap), (x1 - r_cap, y1), (x1 - r_cap, y1 - r_cap)),
        corner((x1 - r_cap, y1), (ix, y1 - r_cap), (x1 - r_cap, y1 - r_cap)),
        line((ix, y1 - r_cap), (ix, iy + r_in)),
        corner((ix, iy + r_in), (ix - r_in, iy), (ix - r_in, iy + r_in)),
        line((ix - r_in, iy), (x0 + r_cap, iy)),
        corner((x0 + r_cap, iy), (x0, iy - r_cap), (x0 + r_cap, iy - r_cap)),
        corner((x0, iy - r_cap), (x0 + r_cap, y0), (x0 + r_cap, iy - r_cap)),
    ]


def fmt(v):
    s = ("%.3f" % v).rstrip("0").rstrip(".")
    return "0" if s in ("", "-0") else s


def emit(subpaths):
    out = []
    for segs in subpaths:
        out.append("M%s %s" % (fmt(segs[0][0][0]), fmt(segs[0][0][1])))
        for p0, c1, c2, p3 in segs:
            if c1 is None:
                out.append("L%s %s" % (fmt(p3[0]), fmt(p3[1])))
            else:
                out.append("C%s %s %s %s %s %s" % (
                    fmt(c1[0]), fmt(c1[1]), fmt(c2[0]), fmt(c2[1]),
                    fmt(p3[0]), fmt(p3[1])))
        out.append("Z")
    return "".join(out)


# --- the icons -----------------------------------------------------------
#
# A subpath wound against the outline is a hole ONLY where it overlaps one.
# Outside, its winding is -1, which is still non-zero, which still fills. So
# "cut this shape but not that one" cannot be said in a single path: the first
# draft of the backup icon subtracted a rounded rect meant for the plate
# behind, and it ate the plate in front and inverted the arrow. At 16 and 32px
# that looked plausible. It only became obvious at 240px.
#
# Hence the hole check below, and hence a plate behind drawn as a bracket that
# never enters the one in front, rather than a full plate cut down to size.


def solid(segs):
    return (segs, False)


def hole(segs):
    return (reverse(segs), True)


def shield_outline():
    """Portrait keyline: 12 wide by 16 tall at 4,2. Clockwise."""
    return [
        line((5.5, 2), (14.5, 2)),
        ((14.5, 2), (15.33, 2), (16, 2.67), (16, 3.5)),
        line((16, 3.5), (16, 9.8)),
        ((16, 9.8), (16, 13.6), (13.4, 16.85), (10.3, 17.95)),
        ((10.3, 17.95), (10.1, 18.02), (9.9, 18.02), (9.7, 17.95)),
        ((9.7, 17.95), (6.6, 16.85), (4, 13.6), (4, 9.8)),
        line((4, 9.8), (4, 3.5)),
        ((4, 3.5), (4, 2.67), (4.67, 2), (5.5, 2)),
    ]


def window_panes(x, y, side, gutter, r):
    """Two by two, knocked out. side must stay at or above the 1.5 minimum."""
    step = side + gutter
    return [hole(rounded_rect(x + dx * step, y + dy * step, side, side, r))
            for dy in (0, 1) for dx in (0, 1)]


SHAPES = {}

# Windows Security: a shield on the portrait keyline carrying the four-pane
# window. The set already has shield, shield_checkmark, shield_keyhole,
# lock_shield and globe_shield, all spoken for by other rows, so a plain
# shield would have swapped one duplicate for another. The panes are what make
# this one Windows Security rather than security in general.
SHAPES["yspot_windows_shield"] = (
    [solid(shield_outline())] + window_panes(6.5, 4.5, 2.75, 1.5, 0.5))

# Windows Backup: a plate with a clockwise arrow knocked out of it, the
# restore, and a bracket behind it, the copy it restores from. The arrowhead
# is the widest point of the knockout and leaves 1.55 of plate outside it; the
# bracket is 2 thick and clears the plate by 1.5. Both are above the minimum.
#
# Four other drawings were tried and thrown away, which is worth recording so
# the next person does not retry them. A box with a down arrow reads as
# "download". An arrow ring around the four panes, and a stack with the arrow
# demoted to a corner modifier badge, both turned to mush below 32px. A plain
# rounded plate with the arrow and nothing behind it is clean, but says
# "refresh", and its silhouette is the same rounded square a dozen icons in the
# set already have -- the bracket is what gives this one a shape of its own at
# a glance, which is what separates two rows in a list.
SHAPES["yspot_backup_restore"] = [
    solid(bracket(5.5, 2, 18, 14.5, 2, 3, 1, 1)),
    solid(rounded_rect(2, 5.5, 12.5, 12.5, 3)),
    hole(clockwise_arrow(8.25, 11.75, 3.0, 1.7, 258, 338, 1.7, 2.8)),
]


# --- checks --------------------------------------------------------------

def _cubic_extrema(p0, c1, c2, p3):
    """Parameters in (0,1) where a cubic turns, on one axis.

    The hull of the four control points would be simpler and is a valid outer
    bound, but it is not tight: the shield's tip has handles 0.02 past the live
    edge while the curve itself stops 0.0025 short of it. A check that fails on
    a shape that is fine gets switched off, so this solves the derivative.
    """
    ts = []
    a = -p0 + 3 * c1 - 3 * c2 + p3
    b = 2 * (p0 - 2 * c1 + c2)
    c = c1 - p0
    if abs(a) < 1e-12:
        if abs(b) > 1e-12:
            ts.append(-c / b)
    else:
        disc = b * b - 4 * a * c
        if disc >= 0:
            root = math.sqrt(disc)
            ts.extend([(-b + root) / (2 * a), (-b - root) / (2 * a)])
    return [t for t in ts if 0.0 < t < 1.0]


def _cubic_at(p0, c1, c2, p3, t):
    u = 1.0 - t
    return u * u * u * p0 + 3 * u * u * t * c1 + 3 * u * t * t * c2 + t * t * t * p3


def bbox(segs):
    """Exact bounding box of one subpath."""
    xs, ys = [], []
    for p0, c1, c2, p3 in segs:
        xs.extend([p0[0], p3[0]])
        ys.extend([p0[1], p3[1]])
        if c1 is None:
            continue
        for i, acc in ((0, xs), (1, ys)):
            a, b, c, d = p0[i], c1[i], c2[i], p3[i]
            for t in _cubic_extrema(a, b, c, d):
                acc.append(_cubic_at(a, b, c, d, t))
    return min(xs), min(ys), max(xs), max(ys)


def union_bbox(parts):
    boxes = [bbox(segs) for segs, _ in parts]
    return (min(b[0] for b in boxes), min(b[1] for b in boxes),
            max(b[2] for b in boxes), max(b[3] for b in boxes))


LIVE = (2.0, 2.0, 18.0, 18.0)
SLACK = 0.01


def inside(inner, outer):
    return (inner[0] >= outer[0] - SLACK and inner[1] >= outer[1] - SLACK
            and inner[2] <= outer[2] + SLACK and inner[3] <= outer[3] + SLACK)


def check():
    """Everything inside the live area, and every hole inside a solid.

    The second half is the one that earns its keep. A hole whose box escapes
    the solid it belongs to is the bug described above SHAPES, and boxes catch
    it: they are coarser than the real geometry, so a hole that passes might
    still be wrong, but one that fails certainly is.
    """
    bad = []
    for name in sorted(SHAPES):
        parts = SHAPES[name]
        box = union_bbox(parts)
        if not inside(box, LIVE):
            bad.append("%s: %s leaves the live area %s"
                       % (name, tuple(round(v, 3) for v in box), LIVE))
        solids = [bbox(segs) for segs, is_hole in parts if not is_hole]
        for segs, is_hole in parts:
            if not is_hole:
                continue
            h = bbox(segs)
            if not any(inside(h, s) for s in solids):
                bad.append("%s: hole %s sits outside every solid, so it fills "
                           "instead of cutting"
                           % (name, tuple(round(v, 3) for v in h)))
    return bad


ICONS = dict((name, emit([segs for segs, _ in parts]))
             for name, parts in SHAPES.items())


# --- output --------------------------------------------------------------

HEADER = '''// YSpot's own icons -- drawn, not vendored.
//
// Windows Security and Windows Backup have no icon in the Fluent UI System
// Icons set, and Microsoft's shipped artwork for them is not ours to
// redistribute, so without these two both rows wear the Windows Settings
// glyph and three rows in the launcher look identical.
//
// Copyright (c) the YSpot authors, under YSpot's own licence. These are
// original drawings. What they take from Microsoft's Fluent iconography is
// its published grid -- a 20 box, a 2 margin, a 16 live area, the keylines
// and the stroke chart, all recorded in docs/icon-grid.md -- so that they sit
// beside the vendored set without looking like guests. A grid is a
// measurement, not artwork.
//
// GENERATED by scripts/draw-yspot-icons.py -- do not edit by hand.

/** viewBox for every path here, the same as the vendored set. */
export const YSPOT_VIEWBOX = "0 0 20 20";

/** Icon key to SVG path data, drawn at the 20px size. */
export const YSPOT_20: Readonly<Record<string, string>> = {
'''


def write_ts(path):
    body = "".join('  %s: "%s",\n' % (k, ICONS[k]) for k in sorted(ICONS))
    io.open(path, "w", encoding="utf-8", newline="\n").write(HEADER + body + "};\n")


PREVIEW = """<!doctype html>
<meta charset="utf-8"><title>YSpot drawn icons</title>
<style>
 body{margin:0;font:13px system-ui;background:#f4f5f7;color:#1b1d21}
 .strip{padding:12px 16px}
 .strip.dark{background:#1c1e22;color:#e7e9ec}
 .row{display:flex;align-items:center;gap:26px;height:64px}
 .row b{width:190px;font-weight:600;font-size:12px}
 svg{display:block}
 .live{fill:none;stroke:#d33;stroke-width:.15;opacity:.55}
</style>
{body}
"""


def preview(path):
    def strip(dark, live):
        rows = []
        for name in sorted(ICONS):
            cells = []
            for px in (16, 20, 32, 48, 96):
                guide = ('<rect class="live" x="2" y="2" width="16" height="16"/>'
                         if live else "")
                cells.append(
                    '<svg width="%d" height="%d" viewBox="0 0 20 20" '
                    'fill="currentColor">%s<path d="%s"/></svg>'
                    % (px, px, guide, ICONS[name]))
            rows.append('<div class="row"><b>%s</b>%s</div>'
                        % (name, "".join(cells)))
        return '<div class="strip%s">%s</div>' % (" dark" if dark else "",
                                                  "".join(rows))

    io.open(path, "w", encoding="utf-8").write(
        PREVIEW.replace("{body}",
                        strip(False, False) + strip(True, False) + strip(False, True)))


def main():
    bad = check()
    for b in bad:
        print("FAIL " + b)
    if bad:
        return 1
    for name in sorted(ICONS):
        print("%-24s ok  %4d chars" % (name, len(ICONS[name])))
    if "--preview" in sys.argv:
        out = sys.argv[sys.argv.index("--preview") + 1]
        preview(out)
        print("preview -> " + out)
    if "--emit" in sys.argv:
        write_ts(OUT)
        print("wrote " + OUT)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
