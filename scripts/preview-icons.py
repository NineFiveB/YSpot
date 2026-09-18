"""Render every vendored icon to an HTML sheet you can open in a browser.

A developer tool, not part of the build. The icons live as path data in
`apps/shell/src/lib/fluentGlyphs.ts` and nothing in the app renders them all,
so there is no way to judge the set, spot a wrong drawing, or check that one
still reads at 16 px. This produces that page.

It shows each icon at the three sizes that matter — 16 for a dense row, 20 for
the authored size, 32 for the launcher's icon box — on both a light and a dark
ground, because a shape that works on one can disappear on the other.

Usage:  python scripts/preview-icons.py [output.html]
"""

import io
import re
import sys
import webbrowser
from pathlib import Path

SRC = Path("apps/shell/src/lib/fluentGlyphs.ts")

PAGE = """<!doctype html>
<meta charset="utf-8">
<title>YSpot icons — {count}</title>
<style>
  :root {{ color-scheme: light dark; --bg:#f4f5f7; --fg:#1b1d21; --muted:#697077;
           --border:rgba(0,0,0,.08); }}
  @media (prefers-color-scheme: dark) {{
    :root {{ --bg:#1c1e22; --fg:#e7e9ec; --muted:#9aa1a9; --border:rgba(255,255,255,.08); }}
  }}
  body {{ margin:0; padding:24px; background:var(--bg); color:var(--fg);
          font:14px "Segoe UI Variable Text","Segoe UI",system-ui,sans-serif; }}
  h1 {{ font-size:18px; font-weight:600; margin:0 0 4px; }}
  p.sub {{ color:var(--muted); margin:0 0 20px; }}
  input {{ font:inherit; padding:6px 10px; width:280px; margin-bottom:20px;
           background:transparent; color:var(--fg);
           border:1px solid var(--border); border-radius:6px; }}
  .grid {{ display:grid; gap:8px;
           grid-template-columns:repeat(auto-fill,minmax(190px,1fr)); }}
  .cell {{ display:flex; align-items:center; gap:12px; padding:10px;
           border:1px solid var(--border); border-radius:8px; }}
  .sizes {{ display:flex; align-items:center; gap:8px; flex:none; }}
  .dark {{ background:#1c1e22; color:#e7e9ec; border-radius:4px;
           padding:3px; display:flex; }}
  .name {{ font-size:12px; color:var(--muted); word-break:break-all; }}
  svg {{ display:block; }}
</style>
<h1>{count} vendored icons</h1>
<p class="sub">Fluent UI System Icons, MIT, &copy; 2020 Microsoft &mdash;
16 / 20 / 32&nbsp;px, then 20&nbsp;px on a dark ground. Follows your system theme.</p>
<input id="q" placeholder="Filter… (e.g. shield, cloud, arrow)" autofocus>
<div class="grid" id="g">{cells}</div>
<script>
  const q = document.getElementById('q'), cells = [...document.querySelectorAll('.cell')];
  q.addEventListener('input', () => {{
    const t = q.value.toLowerCase();
    for (const c of cells) c.hidden = !c.dataset.name.includes(t);
  }});
</script>
"""

CELL = """<div class="cell" data-name="{name}">
  <span class="sizes">
    {s16}{s20}{s32}<span class="dark">{sdark}</span>
  </span>
  <span class="name">{name}</span>
</div>"""


def svg(path: str, px: int) -> str:
    return (
        f'<svg width="{px}" height="{px}" viewBox="0 0 20 20" fill="currentColor"'
        f' aria-hidden="true"><path d="{path}"/></svg>'
    )


def main() -> int:
    if not SRC.exists():
        print(f"{SRC} not found - run from the repo root")
        return 2
    src = io.open(SRC, encoding="utf-8").read()
    icons = re.findall(r'^  ([a-z0-9_]+): "([^"]+)",$', src, re.M)
    if not icons:
        print("no icons parsed - has the generated file changed shape?")
        return 2

    cells = "".join(
        CELL.format(
            name=name,
            s16=svg(d, 16),
            s20=svg(d, 20),
            s32=svg(d, 32),
            sdark=svg(d, 20),
        )
        for name, d in icons
    )
    out = Path(sys.argv[1] if len(sys.argv) > 1 else "icons-preview.html").resolve()
    io.open(out, "w", encoding="utf-8").write(
        PAGE.format(count=len(icons), cells=cells)
    )
    print(f"{len(icons)} icons -> {out}")
    webbrowser.open(out.as_uri())
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
