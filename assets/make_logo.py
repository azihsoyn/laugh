# Renders assets/logo.txt as assets/logo.svg, every glyph on a fixed grid so
# the box-drawing strokes join whatever monospace font the viewer has.
#   python3 assets/make_logo.py
#
# The logo: a review comment whose face has viewed ticks for eyes, beside
# "laugh" with the `gh` hiding in it picked out.
import html

lines = open("assets/logo.txt").read().rstrip("\n").split("\n")
EMBLEM = 11  # columns taken by the comment bubble
WORD = 15  # column where "laugh" starts
GH = WORD + 9  # "g" is the fourth three-column letter

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
    if row == len(lines) - 1:
        return "tag"
    return "gh" if col >= GH else "word"


out = []
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
    if row == len(lines) - 1 and len(line) > WORD:
        # The tagline is set as one string in a smaller size, not on the grid.
        tag = line[WORD:].strip()
        out.append(f'<text class="tag" x="{padx + WORD * cw:.1f}" y="{y:.1f}">{html.escape(tag)}</text>')
        runs = [r for r in runs if r[0] != "tag"]
    for c, text, col in runs:
        xs = " ".join(f"{padx + (col + i) * cw:.1f}" for i in range(len(text)))
        glow = ' filter="url(#glow)"' if c in ("eye", "gh") else ""
        out.append(f'<text class="{c}"{glow} x="{xs}" y="{y:.1f}">{html.escape(text)}</text>')

svg = f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" role="img" aria-label="laugh — the GitHub you'd want in a terminal. A review comment smiling with viewed ticks for eyes, beside the word laugh with the gh inside it highlighted.">
  <style>
    text {{ font-family: "SF Mono", Menlo, Consolas, "DejaVu Sans Mono", "Liberation Mono", monospace; font-size: {fs}px; white-space: pre; }}
    .bubble {{ fill: #7aa2f7; }}
    .eye {{ fill: #9ece6a; font-weight: 700; }}
    .mouth {{ fill: #ff9e64; }}
    .word {{ fill: #c0caf5; }}
    .gh {{ fill: #ff9e64; }}
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
