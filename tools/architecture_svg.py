#!/usr/bin/env python3
"""Draws the README's architecture diagram, in a light and a dark version.

    tools/architecture_svg.py      # writes docs/architecture-{light,dark}.svg

The README picks between them with <picture>, following GitHub's theme.
"""

from pathlib import Path

W, H = 880, 690

THEMES = {
    "light": dict(
        text="#1f2328", muted="#59636e", line="#8c959f",
        card="#f6f8fa", card_stroke="#d1d9e0",
        mesh="#ddf4ff", mesh_stroke="#54aeff", mesh_node="#0969da",
        core="#fff1e5", core_stroke="#e16f24", core_text="#bc4c00",
        inner="#ffffff",
    ),
    "dark": dict(
        text="#e6edf3", muted="#9198a1", line="#6e7681",
        card="#151b23", card_stroke="#3d444d",
        mesh="#0c2d4a", mesh_stroke="#388bfd", mesh_node="#58a6ff",
        core="#2a1708", core_stroke="#db6d28", core_text="#f0883e",
        inner="#0d1117",
    ),
}

SANS = "-apple-system, BlinkMacSystemFont, 'Segoe UI', Helvetica, Arial, sans-serif"
MONO = "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"


def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def text(x, y, s, cls, anchor="middle"):
    return f'<text x="{x}" y="{y}" class="{cls}" text-anchor="{anchor}">{esc(s)}</text>'


def card(x, y, w, h, title, sub, cls="card", title_cls="title", dashed=False):
    dash = ' stroke-dasharray="6 5"' if dashed else ""
    out = [f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="10" class="{cls}"{dash}/>']
    cx = x + w / 2
    if sub:
        out.append(text(cx, y + h / 2 - 4, title, title_cls))
        out.append(text(cx, y + h / 2 + 17, sub, "sub"))
    else:
        out.append(text(cx, y + h / 2 + 6, title, title_cls))
    return out


def arrow(x1, y1, x2, y2, dashed=False, both=False):
    dash = ' stroke-dasharray="5 5"' if dashed else ""
    start = ' marker-start="url(#head)"' if both else ""
    return f'<line x1="{x1}" y1="{y1}" x2="{x2}" y2="{y2}" class="edge"{dash}{start} marker-end="url(#head)"/>'


def draw(t):
    s = []

    # The mesh, with a few radios either side of its label.
    s.append(f'<rect x="40" y="16" width="800" height="80" rx="14" class="mesh"/>')
    s += [text(440, 50, "MeshCore mesh", "title big"), text(440, 74, "nodes · repeaters · room servers", "sub")]
    for side in (0, 1):
        pts = [(80, 60), (130, 38), (170, 72), (225, 44), (275, 68)]
        if side:
            pts = [(W - x, y) for x, y in pts]
        for (x1, y1), (x2, y2) in zip(pts, pts[1:]):
            s.append(f'<line x1="{x1}" y1="{y1}" x2="{x2}" y2="{y2}" class="meshlink"/>')
        for x, y in pts:
            s.append(f'<circle cx="{x}" cy="{y}" r="5" class="meshnode"/>')

    # Two ways in: observer repeaters through a broker, and a radio on USB.
    s.append(arrow(230, 96, 230, 146, dashed=True))
    s.append(arrow(650, 96, 650, 146, dashed=True))
    s.append(text(242, 126, "RF", "label", "start"))
    s.append(text(662, 126, "RF", "label", "start"))

    s += card(60, 150, 340, 64, "Observer repeaters", "observer firmware or meshcoretomqtt")
    s.append(arrow(230, 214, 230, 246))
    s += card(60, 250, 340, 56, "MQTT broker", "")
    s.append(arrow(230, 306, 230, 356))
    s.append(text(242, 336, "the wide view · receive only", "label", "start"))

    s += card(480, 150, 340, 64, "Companion radio", "stock firmware, plugged into USB")
    s.append(arrow(650, 218, 650, 356, both=True))
    s.append(text(662, 272, "USB serial", "label", "start"))
    s.append(text(662, 292, "your identity on the mesh:", "label", "start"))
    s.append(text(662, 312, "send, DMs, rooms", "label", "start"))

    # The server.
    s.append(f'<rect x="40" y="360" width="800" height="152" rx="14" class="core"/>')
    s.append(f'<text x="64" y="392" class="coretitle">ferromeshd'
             f'<tspan class="sub" dx="12">keeps everything, decodes what it can, serves it</tspan></text>')
    boxes = [
        ("raw log", "every record, verbatim"),
        ("SQLite", "packets · messages · nodes"),
        ("HTTP + WebSocket API", "history · live · :7373"),
    ]
    bw, gap, bx = 206, 67, 64
    for i, (title, sub) in enumerate(boxes):
        x = bx + i * (bw + gap)
        s += card(x, 412, bw, 76, title, sub, cls="inner", title_cls="title small")
        if i:
            s.append(arrow(x - gap + 2, 450, x - 4, 450))
    s.append(text(bx + bw + gap / 2, 441, "decode", "label"))

    # Clients, on a bus from the API.
    api_x = bx + 2 * (bw + gap) + bw / 2
    cw, cgap = 188, 16
    centers = [40 + i * (cw + cgap) + cw / 2 for i in range(4)]
    s.append(f'<line x1="{api_x}" y1="512" x2="{api_x}" y2="548" class="edge"/>')
    s.append(f'<line x1="{centers[0]}" y1="548" x2="{centers[-1]}" y2="548" class="edge"/>')
    for cx in centers:
        s.append(arrow(cx, 548, cx, 578))
    s.append(text(api_x - 12, 538, "over the network: clients run anywhere", "label", "end"))

    clients = [
        ("ferromesh", "tail · query · send · health", False),
        ("ferromesh tui", "eight live views · alerts", False),
        ("Your own tools", "JSON history · live events", False),
        ("Planned", "web UI · Home Assistant", True),
    ]
    for i, (title, sub, planned) in enumerate(clients):
        x = 40 + i * (cw + cgap)
        s += card(x, 582, cw, 72, title, sub, cls="card", title_cls="title muted" if planned else "title", dashed=planned)

    style = f"""
    text {{ font-family: {SANS}; fill: {t['text']}; }}
    .title {{ font-size: 17px; font-weight: 600; }}
    .big {{ font-size: 21px; }}
    .small {{ font-size: 16px; }}
    .muted {{ fill: {t['muted']}; }}
    .sub {{ font-size: 13.5px; fill: {t['muted']}; font-weight: 400; }}
    .label {{ font-size: 13.5px; fill: {t['muted']}; }}
    .coretitle {{ font-family: {MONO}; font-size: 20px; font-weight: 700; fill: {t['core_text']}; }}
    .coretitle .sub {{ font-family: {SANS}; }}
    .card {{ fill: {t['card']}; stroke: {t['card_stroke']}; stroke-width: 1.5; }}
    .inner {{ fill: {t['inner']}; stroke: {t['core_stroke']}; stroke-width: 1.2; stroke-opacity: .55; }}
    .mesh {{ fill: {t['mesh']}; stroke: {t['mesh_stroke']}; stroke-width: 1.5; }}
    .meshnode {{ fill: {t['mesh_node']}; }}
    .meshlink {{ stroke: {t['mesh_node']}; stroke-width: 1.5; stroke-opacity: .45; stroke-dasharray: 3 4; }}
    .core {{ fill: {t['core']}; stroke: {t['core_stroke']}; stroke-width: 2; }}
    .edge {{ stroke: {t['line']}; stroke-width: 2; fill: none; }}
    """
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" role="img" aria-label="ferromesh architecture: a MeshCore mesh is heard by observer repeaters publishing to an MQTT broker and by a companion radio on USB; ferromeshd appends every record to a raw log, decodes it into SQLite, and serves an HTTP and WebSocket API to the ferromesh CLI, the terminal UI and your own tools.">
<!-- Generated by tools/architecture_svg.py; edit that instead. -->
<defs>
  <marker id="head" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">
    <path d="M0,0 L10,5 L0,10 z" fill="{t['line']}"/>
  </marker>
  <style>{style}</style>
</defs>
{chr(10).join(s)}
</svg>
"""


if __name__ == "__main__":
    docs = Path(__file__).resolve().parent.parent / "docs"
    for name, theme in THEMES.items():
        (docs / f"architecture-{name}.svg").write_text(draw(theme))
