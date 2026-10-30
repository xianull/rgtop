"""Regenerate the README images: docs/screenshot-{dark,light}.png and docs/demo-{dark,light}.gif.

    cargo build --release
    uv run --with pyte --with pillow --with fonttools docs/screenshots.py [png|gif]

rgtop runs for real in a pseudo-terminal, but `ssh` on its PATH is this
script in --fake-ssh mode: a simulated agent with made-up hosts, users and
jobs (the README must not show real servers). The screen is read with pyte
and drawn with Pillow; block, box-drawing and braille characters are drawn
as shapes so they tile without gaps whatever the font.
"""

import functools, json, math, os, random, select, struct, sys, tempfile, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# ── fake agent ────────────────────────────────────────────────────────────────
HOSTS = {
    "gpu-h100-1": dict(gpu="NVIDIA H100 80GB HBM3", mem=81559, limit=700, idle_w=72, cpu="INTEL(R) XEON(R) PLATINUM 8480+",
                       sockets=2, cores=224, ram=2063, up=41, load=0.21,
                       jobs=[("alice", 96, 72.1, "torchrun --nproc_per_node 4 train.py --model llama-3-70b --bs 8"),
                             ("alice", 95, 71.8, None), ("alice", 97, 72.4, None), ("alice", 94, 71.9, None),
                             ("bob", 42, 31.2, "python -m vllm.entrypoints.openai.api_server --model qwen2.5-32b"),
                             ("carol", 12, 11.6, "python eval.py --suite mmlu --ckpt runs/7b-sft/step-4000"),
                             None, None]),
    "gpu-a100-1": dict(gpu="NVIDIA A100-SXM4-80GB", mem=81920, limit=400, idle_w=58, cpu="AMD EPYC 7763 64-Core Processor",
                       sockets=2, cores=256, ram=1031, up=118, load=0.07,
                       jobs=[("dave", 78, 66.3, "python finetune.py --lora r=64 --data data/chat-200k"),
                             ("dave", 81, 66.0, None), None,
                             ("erin", 23, 18.4, "jupyter-lab --no-browser --port 8890")]),
    "gpu-h200-1": dict(gpu="NVIDIA H200", mem=143771, limit=700, idle_w=80, cpu="INTEL(R) XEON(R) PLATINUM 8570",
                       sockets=2, cores=224, ram=2079, up=12, load=0.35,
                       jobs=[("bob", 99, 128.0, "deepspeed pretrain.py")] * 6 + [None, None]),
    "gpu-l40s-1": dict(gpu="NVIDIA L40S", mem=46068, limit=350, idle_w=36, cpu="AMD EPYC 9354 32-Core Processor",
                       sockets=1, cores=64, ram=515, up=203, load=0.02, jobs=[None] * 4),
}


def fake_agent(host):
    cfg = HOSTS.get(host)
    if cfg is None:
        sys.stderr.write("ssh: Could not resolve hostname %s\n" % host)
        sys.exit(255)
    rnd = random.Random(host)
    n, cores = len(cfg["jobs"]), cfg["cores"]
    util = [j[1] if j else 0.0 for j in cfg["jobs"]]
    jiff, idle, t_prev = [0] * 1, [0], time.time()
    pids = {k: 3000000 + rnd.randrange(900000) for k in range(n)}
    start = time.time()
    out = sys.stdout

    def stat_line():
        return "cpu  %d 0 0 %d 0 0 0 0 0 0" % (jiff[0] - idle[0], idle[0])

    hot = set(rnd.sample(range(cores), max(4, int(cores * cfg["load"] * 0.6))))
    for line in sys.stdin:
        req = line.strip()
        if req == "k":
            continue
        now = time.time()
        dt = max(now - t_prev, 0.05)
        t_prev = now
        prev = stat_line()
        busy = min(0.95, max(0.0, cfg["load"] + rnd.uniform(-0.03, 0.03)))
        jiff[0] += int(cores * 100 * dt)
        idle[0] += int(cores * 100 * dt * (1 - busy))
        w = ["@@B\n", "@@S %s\n" % prev]
        for k, job in enumerate(cfg["jobs"]):
            base = job[1] if job else 0.0
            util[k] = min(100.0, max(0.0, util[k] + rnd.uniform(-6, 6) + (base - util[k]) * 0.3)) if job else 0.0
            u = util[k]
            used = (job[2] * 1024 + rnd.uniform(-60, 60)) if job else rnd.choice([0, 1, 4])
            temp = 30 + u * 0.28 + rnd.uniform(-0.6, 0.6)
            power = cfg["idle_w"] + u / 100 * (cfg["limit"] - cfg["idle_w"]) * 0.85 + rnd.uniform(-4, 4)
            w.append("%d, GPU-%s-%d, %s, %d, %d, %d, %d, %.2f, %.2f\n" % (
                k, host, k, cfg["gpu"], round(u), used, cfg["mem"], temp, power, cfg["limit"]))
        w.append("@@G 0\n@@S %s\n" % stat_line())
        w.append("@@L %.2f %.2f %.2f 3/4000 1\n" % (cores * busy * 1.05, cores * busy, cores * busy * 0.97))
        total = cfg["ram"] * 1048576
        w.append("@@M %d %d\n@@W %d %d %d\n" % (total, total * (0.82 - cfg["load"] * 0.3), total * 0.45, 8 << 20, (8 << 20) - 300000))
        lv = "".join("0123456789abcdefghijklmnopqrstuvwxyz"[min(35, max(0, int(rnd.gauss(30, 4) if c in hot else rnd.gauss(2, 2))))]
                     for c in range(cores))
        w.append("@@N %d\n@@U alice\n@@C %s\n@@T %.0f\n" % (cores, lv, 44 + busy * 30))
        w.append("@@O %d\n@@I %d %s\n" % (cfg["up"] * 86400 + now - start, cfg["sockets"], cfg["cpu"]))
        if req != "s":
            w.append("@@X\n")
            seen = {}
            for k, job in enumerate(cfg["jobs"]):
                if not job:
                    continue
                key = next((i for i, j in enumerate(cfg["jobs"]) if j and j[0] == job[0] and j[3]), k)
                pid = pids[key]
                w.append("@@A GPU-%s-%d, %d, %d\n" % (host, k, pid, job[2] * 1024))
                if pid not in seen and cfg["jobs"][key][3]:
                    seen[pid] = 1
                    age = 3600 * (3 + key * 5) + (now - start)
                    w.append("@@P %d %s %d %s\n" % (pid, job[0], age, cfg["jobs"][key][3]))
                    w.append("@@Q %d %.1f %d\n" % (pid, rnd.uniform(95, 110) * (4 if key == 0 else 1), (12 + key * 7) << 20))
            if req == "h":
                users = {"root": [11.0, 3]}
                for k, job in enumerate(cfg["jobs"]):
                    if job:
                        u = users.setdefault(job[0], [0.0, 0])
                        u[0] += util[k] * 4.6  # dataloader workers
                        u[1] += int(job[2] * 0.8)
                for user, (c, rss) in sorted(users.items(), key=lambda e: -e[1][0]):
                    w.append("@@H %s %.0f %d\n" % (user, c * rnd.uniform(0.95, 1.05), rss << 20))
        w.append("@@E\n")
        time.sleep(rnd.uniform(0.03, 0.05))  # a LAN-ish round trip, so the card shows one
        out.write("".join(w))
        out.flush()


# ── screen capture ────────────────────────────────────────────────────────────
def snap(screen):
    return [[screen.buffer[y][x] for x in range(screen.columns)] for y in range(screen.lines)]


def capture(binary, theme, cols, rows, secs, keys=(), record_from=None, fps=10):
    """Run rgtop for secs, typing keys ((time, bytes) pairs). From record_from on, also
    snapshot the screen fps times a second, never in the middle of a frame."""
    import fcntl, pty, termios
    import pyte
    tmp = tempfile.mkdtemp(prefix="rgtop-shots-")
    os.makedirs(os.path.join(tmp, "bin"))
    os.makedirs(os.path.join(tmp, "cfg", "rgtop"))
    with open(os.path.join(tmp, "bin", "ssh"), "w") as f:
        f.write('#!/bin/sh\nfor a; do h=$a; shift; [ "$h" = "--" ] && break; done\n'
                'exec "%s" "%s" --fake-ssh "$1"\n' % (sys.executable, os.path.abspath(__file__)))
    os.chmod(os.path.join(tmp, "bin", "ssh"), 0o755)
    with open(os.path.join(tmp, "cfg", "rgtop", "hosts"), "w") as f:
        f.write(" ".join(HOSTS) + "\n")
    with open(os.path.join(tmp, "cfg", "rgtop", "settings.json"), "w") as f:
        json.dump({"interval_ms": 300, "theme": theme, "show_proc": True, "show_user": True, "show_hist": True,
                   "show_cores": True, "mouse": False, "collapsed": ["gpu-h200-1", "gpu-l40s-1"]}, f)
    screen = pyte.Screen(cols, rows)
    stream = pyte.ByteStream(screen)
    pid, fd = pty.fork()
    if pid == 0:
        os.environ.update(PATH=os.path.join(tmp, "bin") + ":" + os.environ["PATH"], XDG_CONFIG_HOME=os.path.join(tmp, "cfg"),
                          COLORTERM="truecolor", TERM="xterm-256color", RGTOP_NO_UPDATE_CHECK="1")
        os.execv(binary, [binary])
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    frames, keys, shot, in_frame, t0 = [], list(keys), record_from, False, time.time()
    while (t := time.time() - t0) < secs:
        if keys and t >= keys[0][0]:
            os.write(fd, keys.pop(0)[1])
        if shot is not None and t >= shot and not in_frame:
            frames.append(snap(screen))
            shot += 1 / fps
        if select.select([fd], [], [], 0.01)[0]:
            try:
                data = os.read(fd, 1 << 16)
            except OSError:
                break
            stream.feed(data)
            begin, end = data.rfind(b"\x1b[?2026h"), data.rfind(b"\x1b[?2026l")  # synchronized update
            if begin != end:
                in_frame = begin > end
    os.write(fd, b"q")
    time.sleep(0.5)
    return snap(screen), frames


# ── rendering ─────────────────────────────────────────────────────────────────
TERM_BG = {"dark": "292828", "light": "fbf1c7"}  # gruvbox-material, like the palettes
TERM_FG = {"dark": "d4be98", "light": "654735"}
FONTS = [os.path.expanduser("~/Library/Fonts/JetBrainsMonoNerdFont-%s.ttf"), "/System/Library/Fonts/Menlo.ttc",
         "/System/Library/Fonts/Apple Symbols.ttf", os.path.expanduser("~/Library/Fonts/Arial Unicode.ttf")]


def rgb(h, default):
    h = default if h in ("default", "", None) else h
    try:
        return tuple(int(h[i:i + 2], 16) for i in (0, 2, 4))
    except ValueError:
        return rgb(default, default)


PAGE_BG = {"dark": (13, 17, 23), "light": (255, 255, 255)}  # github.com, under the GIF's rounded corners


@functools.lru_cache
def load_fonts(size):
    from PIL import ImageFont
    from fontTools.ttLib import TTFont
    fonts = []
    for p in FONTS:
        for bold in (False, True):
            fp = p % ("Bold" if bold else "Regular") if "%s" in p else p
            if os.path.exists(fp):
                cmap = TTFont(fp, fontNumber=0).getBestCmap()
                fonts.append((bold, ImageFont.truetype(fp, size), set(cmap)))
    if not fonts:
        sys.exit("no usable font found")
    return fonts


def draw(rows, theme, size):
    from PIL import Image, ImageDraw
    fonts = load_fonts(size)
    lines, columns = len(rows), len(rows[0])
    cw, ch = round(fonts[0][1].getlength("M")), round(size * 1.24)
    pad, bar = size, round(size * 1.6)
    W, H = columns * cw + 2 * pad, lines * ch + 2 * pad + bar
    fg0, bg0 = TERM_FG[theme], TERM_BG[theme]
    img = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    d.rounded_rectangle((0, 0, W - 1, H - 1), radius=size // 2, fill=rgb(bg0, bg0) + (255,))
    for k, c in enumerate(("ff5f57", "febc2e", "28c840")):  # window buttons
        x, y, r = pad + k * size, bar // 2 + 4, size // 4
        d.ellipse((x - r, y - r, x + r, y + r), fill=rgb(c, c))
    stroke = max(2, size // 14)
    gap = max(2, ch // 12)  # between rows, so stacked meters don't merge into one slab

    def glyph_font(chr_, bold):
        for b, f, cmap in fonts:
            if b == bold and ord(chr_) in cmap:
                return f
        for b, f, cmap in fonts:
            if ord(chr_) in cmap:
                return f
        return fonts[0][1]

    for y in range(lines):
        row = rows[y]
        for x in range(columns):
            cell = row[x]
            X, Y = pad + x * cw, pad + bar + y * ch
            bg = rgb(cell.bg, bg0)
            if cell.bg not in ("default", "", None):
                d.rectangle((X, Y + gap, X + cw - 1, Y + ch - 1), fill=bg)
            c, fg = cell.data, rgb(cell.fg, fg0)
            o = ord(c) if c else 32
            if c in ("", " "):
                continue
            if c == "█":
                d.rectangle((X, Y + gap, X + cw - 1, Y + ch - 1), fill=fg)
            elif 0x2589 <= o <= 0x258F:  # ▉ … ▏ left eighths
                d.rectangle((X, Y + gap, X + round(cw * (0x2590 - o) / 8) - 1, Y + ch - 1), fill=fg)
            elif 0x2581 <= o <= 0x2587:  # ▁ … ▇ lower eighths
                d.rectangle((X, Y + ch - round((ch - gap) * (o - 0x2580) / 8), X + cw - 1, Y + ch - 1), fill=fg)
            elif c in "─│╭╮╰╯":
                mx, my = X + cw // 2, Y + ch // 2
                if c == "─":
                    d.rectangle((X, my - stroke // 2, X + cw, my + (stroke - 1) // 2), fill=fg)
                elif c == "│":
                    d.rectangle((mx - stroke // 2, Y, mx + (stroke - 1) // 2, Y + ch), fill=fg)
                else:
                    r = cw // 2
                    right, down = c in "╭╰", c in "╭╮"  # where the corner's arms go
                    cx = mx + (r if right else -r)
                    cy = my + (r if down else -r)
                    start = {"╭": 180, "╮": 270, "╰": 90, "╯": 0}[c]
                    d.arc((cx - r, cy - r, cx + r, cy + r), start, start + 90, fill=fg, width=stroke)
                    vy = (my + r, Y + ch) if down else (Y, my - r)
                    d.rectangle((mx - stroke // 2, vy[0], mx + (stroke - 1) // 2, vy[1]), fill=fg)
                    hx = (mx + r, X + cw) if right else (X, mx - r)
                    d.rectangle((hx[0], my - stroke // 2, hx[1], my + (stroke - 1) // 2), fill=fg)
            elif 0x2800 <= o <= 0x28FF:  # braille: dots 1-2-3-7 left column, 4-5-6-8 right
                bits = o - 0x2800
                pos = [(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2), (0, 3), (1, 3)]
                rr = max(2, cw // 8)
                for b, (cx, cy) in enumerate(pos):
                    if bits >> b & 1:
                        px, py = X + cw * (0.3 + 0.4 * cx), Y + ch * (0.14 + 0.24 * cy)
                        d.ellipse((px - rr, py - rr, px + rr, py + rr), fill=fg)
            else:
                f = glyph_font(c, bool(cell.bold))
                d.text((X, Y + ch / 2), c, font=f, fill=fg, anchor="lm")
    return img


def render(rows, theme, path, size=28):
    from PIL import Image
    img = draw(rows, theme, size)
    img.quantize(256, method=Image.Quantize.FASTOCTREE).save(path, optimize=True)
    print("wrote", os.path.relpath(path, ROOT), "%dx%d" % img.size)


def render_gif(frames, theme, path, fps, size=16):
    from PIL import Image
    imgs = []
    for rows in frames:
        img = draw(rows, theme, size)
        imgs.append(Image.alpha_composite(Image.new("RGBA", img.size, PAGE_BG[theme] + (255,)), img).convert("RGB"))
    # one palette for every frame and no dithering: unchanged pixels stay identical, so each
    # GIF frame stores only the rectangle that changed
    sample = imgs[::max(1, len(imgs) // 8)]
    strip = Image.new("RGB", (imgs[0].width, imgs[0].height * len(sample)))
    for k, img in enumerate(sample):
        strip.paste(img, (0, k * img.height))
    pal = strip.quantize(256, method=Image.Quantize.FASTOCTREE)
    ps = [img.quantize(palette=pal, dither=Image.Dither.NONE) for img in imgs]
    ps[0].save(path, save_all=True, append_images=ps[1:], duration=1000 // fps, loop=0)
    print("wrote", os.path.relpath(path, ROOT), "%dx%d, %d frames, %.1f MB" % (
        *imgs[0].size, len(ps), os.path.getsize(path) / 1e6))


def main():
    if len(sys.argv) > 2 and sys.argv[1] == "--fake-ssh":
        fake_agent(sys.argv[2])
        return
    binary = os.path.join(ROOT, "target", "release", "rgtop")
    what = sys.argv[1:] or ["png", "gif"]
    for theme in ("dark", "light"):
        if "png" in what:
            rows, _ = capture(binary, theme, cols=120, rows=42, secs=25)
            render(rows, theme, os.path.join(ROOT, "docs", "screenshot-%s.png" % theme))
        if "gif" in what:
            # 15 s to fill the history graphs, then 13 s recorded: unfold gpu-h200-1, list view,
            # back to where it started so the loop doesn't jump
            keys = [(19, b"3"), (23, b"l"), (25, b"l"), (25.1, b"3")]
            _, frames = capture(binary, theme, cols=120, rows=42, secs=28, keys=keys, record_from=15)
            render_gif(frames, theme, os.path.join(ROOT, "docs", "demo-%s.gif" % theme), fps=10)


if __name__ == "__main__":
    main()
