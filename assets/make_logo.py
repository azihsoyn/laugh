# Renders assets/logo.txt as assets/logo.svg, every glyph on a fixed grid so
# the box-drawing strokes join whatever monospace font the viewer has.
#   python3 assets/make_logo.py
#
# The logo: a review comment smiling with two Viewed ticks for eyes, beside a
# one-line diff that turns a frown into a laugh, with the `gh` hiding in the
# name picked out. Colours are the app's own: blue for structure, green for
# viewed and added, red for removed, orange for the brand.
import html

lines = open("assets/logo.txt").read().rstrip("\n").split("\n")
EMBLEM = 11  # columns taken by the comment bubble
DIFF = 15  # column where the diff lines start
DEL_ROW, ADD_ROW = 1, 2
BAND = 13  # columns the diff lines' tint covers

fs = 20
cw = fs * 0.6
lh = fs * 1.17
padx, pady = 30, 26
cols = max(len(l) for l in lines)
W = round(padx * 2 + cols * cw)
H = round(pady * 2 + len(lines) * lh + 4)


def cls(row, col, ch):
    if col < EMBLEM:
        if ch == "✓":
            return "eye"
        if row == 2 and 3 <= col <= 7:
            return "mouth"
        return "bubble"
    at = col - DIFF
    if row in (DEL_ROW, ADD_ROW):
        if at < 2:
            return "num"
        if at == 3:
            return "del" if row == DEL_ROW else "add"
        if row == DEL_ROW:
            return "frown"
        return "gh" if at >= 8 else "word"
    return "tag"


out = []
for row, color in ((DEL_ROW, "#3a222a"), (ADD_ROW, "#1d2921")):
    top = pady + row * lh + 1
    out.append(
        f'<rect x="{padx + DIFF * cw - 6:.1f}" y="{top:.1f}" width="{BAND * cw + 12:.1f}" '
        f'height="{lh:.1f}" fill="{color}"/>'
    )

for row, line in enumerate(lines):
    y = pady + row * lh + fs * 0.86
    runs = []
    for col, ch in enumerate(line):
        if ch == " ":
            continue
        c = cls(row, col, ch)
        last = runs[-1] if runs else None
        if last and last[0] == c and last[2] + len(last[1]) == col:
            last[1] += ch
        else:
            runs.append([c, ch, col])
    if row == len(lines) - 1 and len(line) > DIFF:
        # The tagline is set as one string in a smaller size, not on the grid.
        out.append(
            f'<text class="tag" x="{padx + DIFF * cw:.1f}" y="{y:.1f}">{html.escape(line[DIFF:].strip())}</text>'
        )
        runs = [r for r in runs if r[0] != "tag"]
    for c, text, col in runs:
        xs = " ".join(f"{padx + (col + i) * cw:.1f}" for i in range(len(text)))
        glow = ' filter="url(#glow)"' if c in ("eye", "gh") else ""
        out.append(f'<text class="{c}"{glow} x="{xs}" y="{y:.1f}">{html.escape(text)}</text>')

svg = f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" role="img" aria-label="laugh — the GitHub you'd want in a terminal. A review comment smiling with two Viewed ticks for eyes, beside a diff that changes frown to laugh, with the gh in laugh highlighted.">
  <style>
    text {{ font-family: "SF Mono", Menlo, Consolas, "DejaVu Sans Mono", "Liberation Mono", monospace; font-size: {fs}px; white-space: pre; }}
    .bubble {{ fill: #7aa2f7; }}
    .eye {{ fill: #9ece6a; font-weight: 700; }}
    .mouth {{ fill: #ff9e64; }}
    .num {{ fill: #565f89; }}
    .del {{ fill: #f7768e; }}
    .add {{ fill: #9ece6a; }}
    .frown {{ fill: #7a82a8; }}
    .word {{ fill: #c0caf5; font-weight: 700; }}
    .gh {{ fill: #ff9e64; font-weight: 700; }}
    .tag {{ fill: #7a82a8; font-size: {round(fs * 0.8)}px; }}
  </style>
  <defs>
    <filter id="glow" x="-100%" y="-100%" width="300%" height="300%">
      <feGaussianBlur stdDeviation="2.5" result="b"/>
      <feMerge><feMergeNode in="b"/><feMergeNode in="SourceGraphic"/></feMerge>
    </filter>
  </defs>
  <rect x="1" y="1" width="{W - 2}" height="{H - 2}" rx="14" fill="#1a1b26" stroke="#292e42" stroke-width="2"/>
""" + "\n".join("  " + o for o in out) + "\n</svg>\n"
open("assets/logo.svg", "w").write(svg)
print(W, H)
