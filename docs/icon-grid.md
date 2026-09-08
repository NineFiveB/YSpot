# The Fluent icon grid, as YSpot draws to it

Every icon YSpot draws itself must sit on the same grid as the hundred it
vendors, or the set will not look like a set. This records the parameters, and
where each number came from, so a future drawing does not have to re-derive
them.

Source: *Microsoft Fluent system iconography (Community)*, file
`fiicGwq5tCLKpDg4bndsTr`, page **Creation tools** — the frames named
"Grids & Keylines" (`13549:409`), "Stroke Chart" (`12850:25617`) and
"Icon Modifiers" (`12850:25670`). Read out of the file itself, not from a blog
post about it: the numbers below are the components' actual geometry.

**These are parameters, not artwork.** Nothing from that file is copied into
YSpot. Drawing an original shape to a published grid is what the grid is for.

## Box, margin, live area

The box is the full icon; the margin is empty on all four sides; what is left
is the live area every shape must fit inside.

| Icon size | Box | Margin | Live area |
|---|---|---|---|
| 12 | 12 | 1 | 10 |
| 16 | 16 | 1 | 14 |
| **20** | **20** | **2** | **16** |
| 24 | 24 | 2 | 20 |
| 28 | 28 | 2 | 24 |
| 32 | 32 | 2 | 28 |
| 48 | 48 | 4 | 40 |

YSpot authors at **20**, because that is the size the vendored set is taken at
and the size `FLUENT_VIEWBOX` declares. So: a 20x20 box, a 2px margin, and
everything inside `2,2 - 18,18`.

## Stroke weight

The chart gives two weights per size. Regular is the outlined style; Filled is
the minimum weight any *feature* may be in a solid icon — a gap, a knockout, a
crossbar. Going under it is how a shape turns to mush at 16px.

| Icon size | Regular | Filled (minimum feature) |
|---|---|---|
| 12 | 1 | 1.5 |
| 16 | 1 | 1.5 |
| **20** | **1** | **1.5** |
| 24 | 1.5 | 2 |
| 28 | 1.5 | 2 |
| 32 | 2 | 2.5 |
| 48 | 2.5 | 3 |

The vendored files are named `_20_regular` and *are* the Regular style: every
one of them is a single filled path with no `stroke` attribute, which
`vendor-fluent-icons.py` asserts, so the outline has been flattened before it
reaches us. YSpot's renderer applies `fill` and nothing else, so an original
icon has to be flattened the same way — a centreline, offset half a stroke
each side, emitted as the ring between the two.

**The style is not optional.** The first drawing of both YSpot icons was a
solid silhouette rather than an outline, and it passed every check: on the
grid, inside the live area, legible at 16px. Beside the vendored icons at 32px
it read markedly heavier and plainly belonged to a different set — which is
the one thing the grid exists to prevent, and the one thing no check on a
single icon can see. `python scripts/draw-yspot-icons.py --compare out.html`
puts the drawn icons next to their nearest vendored neighbours, which is where
that is visible.

## Keylines

A shape that fills its keyline looks the same visual size as every other icon.
A circle drawn to a square's bounds looks too big; that is the whole point.
Values are the Regular (1px centre stroke) keylines at 20, with the outer bound
the stroke actually reaches:

| Keyline | Path bounds at 20 | Outer bound | Corner radius |
|---|---|---|---|
| Circle | 15 x 15 at 2.5, 2.5 | 16 x 16 at 2, 2 | — |
| Square | 13 x 13 at 3.5, 3.5 | 14 x 14 at 3, 3 | 2.5 (sharp: 1.5) |
| Rect landscape | 15 x 11 at 2.5, 4.5 | 16 x 12 at 2, 4 | 2.5 (sharp: 1.5) |
| Rect portrait | 11 x 15 at 4.5, 2.5 | 12 x 16 at 4, 2 | 2.5 (sharp: 1.5) |

A shield is a portrait shape and takes the portrait keyline. A drive, a card or
a window is landscape.

## Modifiers

A small badge in a corner, knocked out of the base glyph by a ring so it stays
legible against it. At 20: badge **9 x 9**, cutout **11 x 11** — a **1px**
knockout ring. (At 24: 11 and 13, the same 1px ring.)

The file's own warning is worth keeping: *"it's paramount to assess if your
icon needs a modifier ... Modifiers aren't fully accessible, so use them
sparingly."* A modifier costs the base glyph a third of its live area, so it
has to earn that.

## Drawing to it

`scripts/draw-yspot-icons.py` holds the two icons YSpot draws itself, and
`--emit` regenerates `apps/shell/src/lib/yspotGlyphs.ts` from them. CI re-runs
it and fails on a diff, because a drawing that has drifted from the script
that made it fails as a wrong picture, not as an error.

One thing to know before adding a third. **A subpath wound against the outline
is a hole only where it overlaps one.** Outside, its winding is -1, which is
still non-zero, which still fills. So "cut this shape but not that one" is not
expressible in a single filled path, and the boolean subtraction that would
express it has to be done before the path is written — or avoided, by drawing
the shape you actually want.

The backup icon's first draft got this wrong: a rounded rect meant to trim the
plate behind also ate the plate in front and inverted the arrow. It looked
plausible at 16, 20, 32 and 48px, and was obviously broken at 240. The script
now refuses to emit a hole whose bounding box escapes every solid, which is a
coarse check — a hole that passes it can still be wrong — but the one that
failed was certainly wrong.

Turning a centreline into a 1px ring is exact for a rounded rectangle: inset
the box by the stroke and the radius with it. For anything curved it is
Tiller-Hanson, which is an approximation, so the script measures what it
produced — worst distance from the centreline to either offset, against half a
stroke — and halves a segment until that is under 0.02. On the shield's long
bottom curves one undivided segment came out 0.135 wide.

Two things that check got wrong before it was right, both worth knowing if you
touch it: measuring to the nearest *sampled point* rather than to the curve
reports the sampling chord as error (0.078 on a straight edge, most of a
tolerance), and a tolerance loose enough to hide that is loose enough to hide
the real thing.

## Checking the result

`python scripts/preview-icons.py` renders every vendored glyph at 16 / 20 / 32
and on a dark ground; `python scripts/draw-yspot-icons.py --preview out.html`
does the same for the drawn ones, and adds a 96px column with the live area
marked. An icon that only works at 32 has failed — and so has one that only
looks right small, which is the harder case to notice.
