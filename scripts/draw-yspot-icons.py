"""Draw YSpot's own icons on the Fluent 20px grid.

Some rows have no icon of their own. Windows Security and Windows Backup both
fall back to the Windows Settings glyph, so three rows in the launcher look
identical; YSuite Settings would make a fourth. Fluent's MIT set has no icon
for any of them, and Microsoft's shipped artwork is not ours to copy. So these
are drawn.

Every number here comes from docs/icon-grid.md, which was read out of the
Fluent iconography community file itself: a 20 box, a 2 margin, a 16 live area,
1px Regular stroke, 1.5px minimum feature, and the keylines a shape must fill
to look the same visual size as its neighbours. Drawing an original shape to a
published grid is what a published grid is for. Nothing is copied from that
file -- these drawings are YSpot's, under YSpot's licence.

The set we vendor is the Regular style: outlines at a 1px stroke, shipped as a
single filled path because the stroke has been outlined before export. So
these are drawn the same way -- a centreline, offset half a stroke either side,
emitted as a ring. The first version of the first two was a solid silhouette
instead, which is a different family: side by side with the vendored icons at
32px it read markedly heavier, which is the whole thing the grid exists to
prevent. `--compare` puts them next to their neighbours so that is checkable
rather than remembered.

Usage:
    python scripts/draw-yspot-icons.py                   check and report
    python scripts/draw-yspot-icons.py --emit            write the TS module
    python scripts/draw-yspot-icons.py --preview p.html  a sheet to look at
    python scripts/draw-yspot-icons.py --compare c.html  beside the vendored set
"""

import io
import math
import re
import sys

OUT = "apps/shell/src/lib/yspotGlyphs.ts"
VENDORED = "apps/shell/src/lib/fluentGlyphs.ts"

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
    r3 = math.hypot(p3[0] - centre[0], p3[1] - centre[1])
    # Both ends must be the same distance from the centre or this is not an
    # arc, and what comes out is a lopsided curve that still offsets to a
    # clean 1px stroke -- so the stroke check cannot see it. The shield's
    # top-left corner was built this way, 1.5 across and 2.5 down, and the
    # only symptom was that the two shoulders did not match.
    if abs(r - r3) > 1e-6:
        raise ValueError("corner %s -> %s about %s is not an arc: radii %.4f "
                         "and %.4f" % (p0, p3, centre, r, r3))
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


def hexagon(cx, cy, r, fillet):
    """Pointy-top regular hexagon centreline, clockwise, corners filleted.

    Returns (segments, vertices). The vertices come back because the shapes
    drawn inside a hexagon are usually aimed at them.

    Filleted rather than mitred: a 120 degree corner throws a long spike
    through `offset_segs`, and Fluent rounds its corners anyway. The fillet
    pulls the extreme points in a little, so the hexagon that fits the live
    area is slightly smaller than the bare circumradius suggests -- which is
    why `check` measures the result instead of trusting the radius.
    """
    vs = [(cx + r * math.cos(math.radians(a)), cy + r * math.sin(math.radians(a)))
          for a in (-90, -30, 30, 90, 150, 210)]      # T, UR, LR, B, LL, UL
    n = len(vs)
    joints = []
    for i, v in enumerate(vs):
        a, b = vs[(i - 1) % n], vs[(i + 1) % n]
        ua, ub = _unit(v, a), _unit(v, b)
        half = math.acos(max(-1.0, min(1.0, ua[0] * ub[0] + ua[1] * ub[1]))) / 2.0
        d = fillet / math.tan(half)
        bis = _unit((0.0, 0.0), (ua[0] + ub[0], ua[1] + ub[1]))
        reach = fillet / math.sin(half)
        joints.append((
            (v[0] + ua[0] * d, v[1] + ua[1] * d),
            (v[0] + ub[0] * d, v[1] + ub[1] * d),
            (v[0] + bis[0] * reach, v[1] + bis[1] * reach),
        ))
    segs = []
    for i in range(n):
        t_in, t_out, c = joints[i]
        segs.append(corner(t_in, t_out, c))
        segs.append(line(t_out, joints[(i + 1) % n][0]))
    return segs, vs


def spoke(p0, p1, w):
    """A w-wide bar from p0 to p1, wound clockwise so it unions with a ring.

    Under the nonzero rule a bar laid across a ring's hole raises the winding
    there from 0 to 1, which fills it -- no boolean needed, and none possible
    (see the note above SHAPES). Winding is decided by measurement rather than
    by argument order, so a caller cannot get it backwards.
    """
    u = _unit(p0, p1)
    nx, ny, h = u[1], -u[0], w / 2.0
    q = [(p0[0] + nx * h, p0[1] + ny * h), (p1[0] + nx * h, p1[1] + ny * h),
         (p1[0] - nx * h, p1[1] - ny * h), (p0[0] - nx * h, p0[1] - ny * h)]
    segs = [line(q[i], q[(i + 1) % 4]) for i in range(4)]
    return segs if signed_area(segs) > 0 else reverse(segs)


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


# --- turning a centreline into an outline --------------------------------
#
# The Regular style is a 1px stroke, and the renderer only fills, so a stroke
# has to be emitted as the ring between two offset copies of its centreline.
# For a rounded rectangle the offset is exact -- inset the box by the stroke
# and the radius with it -- so outlined_rect does that directly. For the
# shield it is Tiller-Hanson: offset each leg of a segment's control polygon
# along that leg's own normal and intersect neighbours for the new handles.
# That is an approximation, so check_stroke_width measures the result rather
# than trusting it.

def _at(p0, c1, c2, p3, t):
    u = 1.0 - t
    return u * u * u * p0 + 3 * u * u * t * c1 + 3 * u * t * t * c2 + t * t * t * p3


def _point(seg, t):
    p0, c1, c2, p3 = seg
    if c1 is None:
        return (p0[0] + (p3[0] - p0[0]) * t, p0[1] + (p3[1] - p0[1]) * t)
    return (_at(p0[0], c1[0], c2[0], p3[0], t),
            _at(p0[1], c1[1], c2[1], p3[1], t))


def _sample(segs, steps):
    return [_point(s, i / float(steps)) for s in segs for i in range(steps)]


def _unit(a, b):
    dx, dy = b[0] - a[0], b[1] - a[1]
    n = math.hypot(dx, dy)
    return None if n < 1e-12 else (dx / n, dy / n)


def _normal(u):
    return (u[1], -u[0])


def _shift(p, n, d):
    return (p[0] + n[0] * d, p[1] + n[1] * d)


def _meet(a0, a1, b0, b1, fallback):
    """Where line a0-a1 meets b0-b1, or fallback if they are parallel."""
    d1 = (a1[0] - a0[0], a1[1] - a0[1])
    d2 = (b1[0] - b0[0], b1[1] - b0[1])
    den = d1[0] * d2[1] - d1[1] * d2[0]
    if abs(den) < 1e-9:
        return fallback
    t = ((b0[0] - a0[0]) * d2[1] - (b0[1] - a0[1]) * d2[0]) / den
    return (a0[0] + d1[0] * t, a0[1] + d1[1] * t)


def _as_cubic(seg):
    p0, c1, c2, p3 = seg
    if c1 is not None:
        return seg
    third = ((p3[0] - p0[0]) / 3.0, (p3[1] - p0[1]) / 3.0)
    return (p0, (p0[0] + third[0], p0[1] + third[1]),
            (p3[0] - third[0], p3[1] - third[1]), p3)


def _split(seg):
    """de Casteljau at the midpoint. Lines split as lines."""
    p0, c1, c2, p3 = seg
    if c1 is None:
        mid = ((p0[0] + p3[0]) / 2.0, (p0[1] + p3[1]) / 2.0)
        return [line(p0, mid), line(mid, p3)]
    mid2 = lambda a, b: ((a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0)
    a, b, c = mid2(p0, c1), mid2(c1, c2), mid2(c2, p3)
    d, e = mid2(a, b), mid2(b, c)
    m = mid2(d, e)
    return [(p0, a, d, m), (m, e, c, p3)]


def _offset_one(seg, d):
    """Tiller-Hanson on a single segment."""
    p0, c1, c2, p3 = seg
    if c1 is None:
        u = _unit(p0, p3)
        if u is None:
            return None
        n = _normal(u)
        return line(_shift(p0, n, d), _shift(p3, n, d))
    # Collapse a degenerate leg onto its neighbour so the normal exists.
    u1 = _unit(p0, c1) or _unit(p0, c2) or _unit(p0, p3)
    u3 = _unit(c2, p3) or _unit(c1, p3) or _unit(p0, p3)
    if u1 is None or u3 is None:
        return None
    u2 = _unit(c1, c2) or u1
    n1, n2, n3 = _normal(u1), _normal(u2), _normal(u3)
    a0, a1 = _shift(p0, n1, d), _shift(c1, n1, d)
    b0, b1 = _shift(c1, n2, d), _shift(c2, n2, d)
    e0, e1 = _shift(c2, n3, d), _shift(p3, n3, d)
    return (a0, _meet(a0, a1, b0, b1, a1), _meet(b0, b1, e0, e1, e0), e1)


def _offset_error(seg, off, d):
    """Worst gap between a segment and its offset, against |d|."""
    dense = [_point(off, i / 24.0) for i in range(25)]
    worst = 0.0
    for i in range(1, 12):
        q = _point(seg, i / 12.0)
        near = min(math.hypot(q[0] - r[0], q[1] - r[1]) for r in dense)
        worst = max(worst, abs(near - abs(d)))
    return worst


def offset_segs(segs, d, tol=0.01, depth=5):
    """Offset a path by d, positive to the side _normal points.

    Tiller-Hanson is exact only for a straight leg, and on the shield's long
    bottom curves one segment came out 0.135 wide of a 1px stroke. So each
    segment is halved until its own offset measures right, which costs a few
    extra cubics on the curves and nothing at all on the straights.
    """
    out = []
    for seg in segs:
        pieces = [seg]
        for _ in range(depth):
            offs = [_offset_one(p, d) for p in pieces]
            if any(o is None for o in offs):
                break
            if max(_offset_error(p, o, d) for p, o in zip(pieces, offs)) <= tol:
                break
            pieces = [q for p in pieces for q in _split(p)]
        for p in pieces:
            o = _offset_one(p, d)
            if o is not None:
                out.append(o)
    return out


def signed_area(segs, steps=24):
    pts = _sample(segs, steps)
    a = 0.0
    for i in range(len(pts)):
        x0, y0 = pts[i]
        x1, y1 = pts[(i + 1) % len(pts)]
        a += x0 * y1 - x1 * y0
    return a / 2.0


def outlined(segs, w):
    """A closed centreline as the ring between its two offsets.

    Returns (outer, inner-reversed). Which offset is outside depends on the
    winding, so it is decided by area rather than by a sign convention that
    would be one more thing to get backwards.
    """
    a = offset_segs(segs, w / 2.0)
    b = offset_segs(segs, -w / 2.0)
    if abs(signed_area(a)) < abs(signed_area(b)):
        a, b = b, a
    return a, reverse(b)


def outlined_rect(x, y, w, h, r, t):
    """An outlined rounded rectangle. Exact: the inset of a rounded rect is
    a rounded rect, with the radius inset too."""
    return (rounded_rect(x, y, w, h, r),
            reverse(rounded_rect(x + t, y + t, w - 2 * t, h - 2 * t, r - t)))


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
# "cut this shape but not that one" cannot be said in a single path: an early
# draft of the backup icon subtracted a rounded rect meant for the plate
# behind, and it ate the plate in front and inverted the arrow. At 16 and 32px
# that looked plausible. It only became obvious at 240px.
#
# Hence the hole check below, and hence a plate behind drawn as a bracket that
# never enters the one in front, rather than a full plate cut down to size.

STROKE = 1.0  # the Regular weight at 20, from the stroke chart


def solid(segs):
    return (segs, False)


def hole(segs):
    return (reverse(segs), True)


def ring(pair):
    outer, inner = pair
    return [solid(outer), (inner, True)]


def shield_centreline():
    """Portrait keyline: the stroke's centreline, 11 x 15 at 4.5, 2.5.

    Tangent-continuous at every joint, including the nose, because a kink
    would open a hairline gap where the two offsets meet.
    """
    # The nose tangent, and the handle lengths that meet it.
    tx, ty = -0.9439, 0.3304
    reach, nose = 3.1, 0.18
    right = (10.0 - tx * reach, 17.44 - ty * reach)   # handle into the nose
    left = (10.0 + tx * reach, 17.44 - ty * reach)    # its mirror
    return [
        line((7.0, 2.5), (13.0, 2.5)),
        corner((13.0, 2.5), (15.5, 5.0), (13.0, 5.0)),
        line((15.5, 5.0), (15.5, 9.6)),
        ((15.5, 9.6), (15.5, 13.0), (right[0], right[1]), (10.28, 17.44)),
        ((10.28, 17.44), (10.28 + tx * nose, 17.44 + ty * nose),
         (9.72 - tx * nose, 17.44 + ty * nose), (9.72, 17.44)),
        ((9.72, 17.44), (left[0], left[1]), (4.5, 13.0), (4.5, 9.6)),
        line((4.5, 9.6), (4.5, 5.0)),
        corner((4.5, 5.0), (7.0, 2.5), (7.0, 5.0)),
    ]


def window_panes(x, y, side, gutter, r):
    """Two by two, filled, inside an outlined shape. side stays at or above
    the 1.5 minimum feature."""
    step = side + gutter
    return [solid(rounded_rect(x + dx * step, y + dy * step, side, side, r))
            for dy in (0, 1) for dx in (0, 1)]


SHAPES = {}

# Windows Security: an outlined shield carrying the four-pane window, the way
# shield_keyhole carries a filled keyhole. The set already has shield,
# shield_checkmark, shield_keyhole, lock_shield and globe_shield, all spoken
# for by other rows, so a plain shield would have swapped one duplicate for
# another. The panes are what make this one Windows Security rather than
# security in general.
SHAPES["yspot_windows_shield"] = (
    ring(outlined(shield_centreline(), STROKE))
    + window_panes(7.0, 5.75, 2.0, 1.5, 0.4))

# Windows Backup: an outlined plate with a clockwise arrow inside it, the
# restore, and the top and right edges of a second plate behind it, the copy
# it restores from. The plate behind is offset 3.5, so a 1 stroke leaves a
# 2.5 gap on both sides.
#
# Four other drawings were tried and thrown away, which is worth recording so
# the next person does not retry them. A box with a down arrow reads as
# "download". An arrow ring around the four panes, and a stack with the arrow
# demoted to a corner modifier badge, both turned to mush below 32px. A plate
# with the arrow and nothing behind it is clean, but says "refresh", and its
# silhouette is the same rounded square a dozen icons in the set already have
# -- the sheet behind is what gives this one a shape of its own at a glance,
# which is what separates two rows in a list.
SHAPES["yspot_backup_restore"] = (
    [solid(bracket(5.5, 2, 18, 14.5, STROKE, 3, 2, 0.5))]
    + ring(outlined_rect(2, 5.5, 12.5, 12.5, 3, STROKE))
    + [solid(clockwise_arrow(8.25, 11.75, 2.9, STROKE, 258, 338, 1.35, 2.3))])


def suite_cube(r=7.5, fillet=1.0):
    """An isometric cube: three faces of one object, seamed in a Y.

    A cube in isometric projection IS a hexagon, and the three visible faces
    meet at the near corner, which projects to the centre. Seen from above the
    near corner points down, so the seam runs stem-down, arms-up: an upright
    Y. Seen from below it inverts and the mark reads as a flat pinwheel rather
    than a solid -- the depth cue is entirely in which way the Y points, so
    this is not a detail to take on trust. `view` is not a parameter because
    only one of the two is a cube.
    """
    ring_segs, vs = hexagon(10, 10, r, fillet)
    outer, inner = outlined(ring_segs, STROKE)
    if signed_area(outer) < 0:
        outer, inner = reverse(outer), reverse(inner)
    _, upper_right, _, bottom, _, upper_left = vs
    return ([solid(outer), (inner, True)]
            + [solid(spoke((10, 10), tip, STROKE))
               for tip in (bottom, upper_left, upper_right)])


# YSuite Settings: the hub that configures YKeys, YTile and YBar.
#
# Three rows say "Settings" at once -- YSpot's, Windows', and this -- so a
# third gear would have recreated the duplicate-icon problem this whole effort
# started from. The label already carries "settings"; what has to differ is
# the identity, so this mark depicts the suite, not the act of configuring it.
#
# A cube because the three faces are three apps in one object, and because the
# hexagon is a silhouette nothing else in the set owns: every neighbour is a
# rounded rectangle (window, grid, archive, hard_drive) or a circle (the two
# gears), so it separates on outline alone at any size. The Y seam is the
# suite's initial arriving as geometry rather than as a letter, which is the
# only form of it that survives forced-colors and translation.
#
# Rejected, and why, so they are not retried: three tiled panels read well but
# depict TILING, and YTile is one of the three apps this hub configures -- the
# parent would have collided with its own child. Three offset layers are
# `window_multiple`. Three stacked bars are a text-align control. A bar over
# two panels is a titled window. Radial arrangements read as `arrow_sync`.
#
# The cube's honest cost: it is also the universal package/module mark, so it
# borrows a meaning. If YSpot ever grows a plugins row, that row wants this
# icon and this one should move.
SHAPES["yspot_suite_cube"] = suite_cube()


# --- checks --------------------------------------------------------------

def _cubic_extrema(p0, c1, c2, p3):
    """Parameters in (0,1) where a cubic turns, on one axis.

    The hull of the four control points would be simpler and is a valid outer
    bound, but it is not tight, and a check that fails on a shape that is fine
    gets switched off. So this solves the derivative.
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
                acc.append(_at(a, b, c, d, t))
    return min(xs), min(ys), max(xs), max(ys)


def union_bbox(parts):
    boxes = [bbox(segs) for segs, _ in parts]
    return (min(b[0] for b in boxes), min(b[1] for b in boxes),
            max(b[2] for b in boxes), max(b[3] for b in boxes))


LIVE = (2.0, 2.0, 18.0, 18.0)
SLACK = 0.01


def inside(inner, outer, slack=SLACK):
    return (inner[0] >= outer[0] - slack and inner[1] >= outer[1] - slack
            and inner[2] <= outer[2] + slack and inner[3] <= outer[3] + slack)


def _dist_to_polyline(p, pts):
    """Distance to the polyline through pts, treated as a closed loop.

    Measuring to the nearest sampled POINT instead of the nearest segment
    reports the sampling chord as if it were error: on a 7-long straight edge
    at 12 samples that alone read 0.078, which is most of a tolerance.
    """
    best = float("inf")
    n = len(pts)
    for i in range(n):
        a, b = pts[i], pts[(i + 1) % n]
        dx, dy = b[0] - a[0], b[1] - a[1]
        L = dx * dx + dy * dy
        if L < 1e-18:
            t = 0.0
        else:
            t = ((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / L
            t = 0.0 if t < 0.0 else (1.0 if t > 1.0 else t)
        qx, qy = a[0] + dx * t, a[1] + dy * t
        best = min(best, math.hypot(p[0] - qx, p[1] - qy))
    return best


def check_stroke_width(centreline, outer, inner, want, tol=0.02):
    """How far the offset drifted from the stroke it is meant to be.

    Tiller-Hanson is an approximation. Measuring it costs nothing and turns
    "this should be about 1px" into a number that can go red.
    """
    outs, ins = _sample(outer, 10), _sample(inner, 10)
    worst = 0.0
    for p in _sample(centreline, 10):
        d = min(_dist_to_polyline(p, outs), _dist_to_polyline(p, ins))
        worst = max(worst, abs(d - want / 2.0))
    return worst if worst > tol else 0.0


def check_symmetry(segs, axis=10.0, tol=0.01):
    """Worst distance from the shape to its own mirror image.

    The corner assertion catches a corner that is not an arc. This catches
    the rest: a shape meant to be symmetric that is quietly lopsided reads as
    "slightly off" and is very hard to see at 20px.
    """
    pts = _sample(segs, 12)
    mirror = [(2 * axis - x, y) for x, y in pts]
    worst = 0.0
    for p in mirror:
        worst = max(worst, _dist_to_polyline(p, pts))
    return worst if worst > tol else 0.0


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
    # Every curved centreline that gets offset into a stroke, checked for the
    # two things the drawing cannot show you: that the offset really is the
    # weight it claims, and that a shape meant to be symmetric is.
    for icon, centreline in (("yspot_windows_shield", shield_centreline()),
                             ("yspot_suite_cube", hexagon(10, 10, 7.5, 1.0)[0])):
        lop = check_symmetry(centreline)
        if lop:
            bad.append("%s: %.3f off symmetric about x=10" % (icon, lop))
        o, i = outlined(centreline, STROKE)
        drift = check_stroke_width(centreline, o, reverse(i), STROKE)
        if drift:
            bad.append("%s: the offset drifts %.3f from a %s stroke"
                       % (icon, drift, STROKE))
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
// and the 1px Regular stroke, all recorded in docs/icon-grid.md -- so that
// they sit beside the vendored set without looking like guests. A grid is a
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


PAGE = """<!doctype html>
<meta charset="utf-8"><title>{title}</title>
<style>
 body{{margin:0;font:13px system-ui;background:#f4f5f7;color:#1b1d21}}
 .strip{{padding:12px 16px}}
 .strip.dark{{background:#1c1e22;color:#e7e9ec}}
 .row{{display:flex;align-items:center;gap:26px;height:64px}}
 .row b{{width:190px;font-weight:600;font-size:12px}}
 .wrap{{display:flex;flex-wrap:wrap;gap:16px;width:660px}}
 .cell{{text-align:center;width:76px}}
 .cell span{{display:block;font-size:10px;opacity:.6;word-break:break-all}}
 svg{{display:block;margin:0 auto}}
 .live{{fill:none;stroke:#d33;stroke-width:.15;opacity:.55}}
</style>
{body}
"""


def _svg(d, px, live=False):
    guide = '<rect class="live" x="2" y="2" width="16" height="16"/>' if live else ""
    return ('<svg width="%d" height="%d" viewBox="0 0 20 20" fill="currentColor">'
            '%s<path d="%s"/></svg>' % (px, px, guide, d))


def preview(path):
    def strip(dark, live):
        rows = []
        for name in sorted(ICONS):
            cells = "".join(_svg(ICONS[name], px, live)
                            for px in (16, 20, 32, 48, 96))
            rows.append('<div class="row"><b>%s</b>%s</div>' % (name, cells))
        return '<div class="strip%s">%s</div>' % (" dark" if dark else "",
                                                  "".join(rows))

    io.open(path, "w", encoding="utf-8").write(
        PAGE.format(title="YSpot drawn icons",
                    body=strip(False, False) + strip(True, False)
                    + strip(False, True)))


# Nearest neighbours in the vendored set: the shields these two must not be
# mistaken for, and the icons whose weight they have to match.
NEIGHBOURS = ["shield", "shield_checkmark", "shield_keyhole", "lock_shield",
              "settings", "hard_drive", "archive", "arrow_sync", "history",
              "folder", "apps_settings", "grid", "window_multiple",
              "puzzle_piece", "options", "app_generic"]


def compare(path):
    src = io.open(VENDORED, encoding="utf-8").read()
    vendored = dict(re.findall(r'^  ([a-z0-9_]+): "([^"]+)",$', src, re.M))
    rows = [(k, vendored[k]) for k in NEIGHBOURS if k in vendored]
    rows += sorted(ICONS.items())

    def strip(dark):
        cells = "".join('<div class="cell">%s<span>%s</span></div>'
                        % (_svg(d, 32), k) for k, d in rows)
        return ('<div class="strip%s"><div class="wrap">%s</div></div>'
                % (" dark" if dark else "", cells))

    io.open(path, "w", encoding="utf-8").write(
        PAGE.format(title="drawn beside vendored", body=strip(False) + strip(True)))


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
    if "--compare" in sys.argv:
        out = sys.argv[sys.argv.index("--compare") + 1]
        compare(out)
        print("compare -> " + out)
    if "--emit" in sys.argv:
        write_ts(OUT)
        print("wrote " + OUT)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
