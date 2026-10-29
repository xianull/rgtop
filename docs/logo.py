"""Regenerate docs/logo-dark.svg and docs/logo-light.svg.

    python3 docs/logo.py

"RGTOP" in the ANSI Shadow figlet font, like btop's logo. The blocks carry the
meter gradient, green -> yellow -> red from left to right (a bar filling up),
the double-line shadow the same colours, fainter. Everything is drawn as
shapes, so it looks the same whatever fonts the viewer has.
"""
import os

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ART = [
    "██████╗  ██████╗ ████████╗ ██████╗ ██████╗ ",
    "██╔══██╗██╔════╝ ╚══██╔══╝██╔═══██╗██╔══██╗",
    "██████╔╝██║  ███╗   ██║   ██║   ██║██████╔╝",
    "██╔══██╗██║   ██║   ██║   ██║   ██║██╔═══╝ ",
    "██║  ██║╚██████╔╝   ██║   ╚██████╔╝██║     ",
    "╚═╝  ╚═╝ ╚═════╝    ╚═╝    ╚═════╝ ╚═╝     ",
]
W, H, D = 10, 20, 1.6  # cell width and height (a terminal cell is ~1:2); half the gap of a double line
# the meter ramp of src/theme.rs (gruvbox-material green, yellow, red)
PALETTES = {"dark": ("#a9b665", "#d8a657", "#ea6962"), "light": ("#6c782e", "#b47109", "#c14a4a")}


def shadow(ch, x, y):
    """Double-line box drawing as two polylines, joined like a terminal font draws them."""
    l, r, t, b, cx, cy = x, x + W, y, y + H, x + W / 2, y + H / 2
    return {
        "═": [[(l, cy - D), (r, cy - D)], [(l, cy + D), (r, cy + D)]],
        "║": [[(cx - D, t), (cx - D, b)], [(cx + D, t), (cx + D, b)]],
        "╗": [[(l, cy - D), (cx + D, cy - D), (cx + D, b)], [(l, cy + D), (cx - D, cy + D), (cx - D, b)]],
        "╔": [[(r, cy - D), (cx - D, cy - D), (cx - D, b)], [(r, cy + D), (cx + D, cy + D), (cx + D, b)]],
        "╝": [[(l, cy + D), (cx + D, cy + D), (cx + D, t)], [(l, cy - D), (cx - D, cy - D), (cx - D, t)]],
        "╚": [[(r, cy + D), (cx - D, cy + D), (cx - D, t)], [(r, cy - D), (cx + D, cy - D), (cx + D, t)]],
    }[ch]


def svg(colors):
    cols = max(len(r) for r in ART)
    w, h = cols * W, len(ART) * H
    blocks, lines = [], []
    for row, text in enumerate(ART):
        y, start = row * H, None
        for col, ch in enumerate(text + " "):
            if ch == "█" and start is None:
                start = col
            elif ch != "█" and start is not None:  # one rect per run of blocks
                blocks.append('<rect x="%g" y="%g" width="%g" height="%g"/>' % (start * W, y, (col - start) * W, H))
                start = None
            if ch in "═║╗╔╝╚":
                for pl in shadow(ch, col * W, y):
                    lines.append('<polyline points="%s"/>' % " ".join("%g,%g" % p for p in pl))
    stops = "".join('<stop offset="%g" stop-color="%s"/>' % (k / 2, c) for k, c in enumerate(colors))
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w:g} {h:g}" width="{w:g}" height="{h:g}" role="img">
<title>rgtop</title>
<defs><linearGradient id="ramp" gradientUnits="userSpaceOnUse" x1="0" y1="0" x2="{w:g}" y2="0">{stops}</linearGradient></defs>
<g fill="url(#ramp)" shape-rendering="crispEdges">{"".join(blocks)}</g>
<g fill="none" stroke="url(#ramp)" stroke-width="1.1" stroke-opacity="0.7" stroke-linejoin="miter">{"".join(lines)}</g>
</svg>
"""


if __name__ == "__main__":
    for name, colors in PALETTES.items():
        path = os.path.join(ROOT, "docs", "logo-%s.svg" % name)
        with open(path, "w") as f:
            f.write(svg(colors))
        print("wrote", os.path.relpath(path, ROOT))
