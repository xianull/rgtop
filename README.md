# rgtop

**English** | [简体中文](README.zh-CN.md)

A btop-style live dashboard for NVIDIA GPUs, CPUs and memory on many remote servers, over plain SSH. One ~700 KB static binary on your machine; nothing to install on the servers.

```
 ◆ rgtop   11/40 GPUs idle  ·  2,282 GiB VRAM free  ·  5/5 hosts  ·  you on 4 GPUs   ● idle  ● light  ● busy    20:57:39
╭─ ▾ ● gpu-h100-1  8× H100 80GB HBM3 · 2× XEON PLATINUM 8558 ─────────────────────────────────── 4/8 idle · 499G free ─╮
│ CPU ███▊                      16%  RAM ██▊▒▒▒▒▒▒▒            275/2016G  swap 0.2G       ⣀⣠⣤⣴⣶⣦⣤⣀⣀⣠⣤⣴   45°       │
│ 192c ▁▂█▁▁▁█▁█▁▁▁▁▁▁▁▁▁▂█▁▁▁▁█▂▁█▁▁▃█▁▁▁███▁▃▁▁██▁▁█▁█▃▁▁▃█▃█▄▁▃▁▁█▁▁                            load 40.9 33.1 30.7 │
│   # UTIL                           VRAM                                  USER         HISTORY         TEMP POWER │
│ ● 0 ████████████████████████ 100%  █████████████████████▊     72.1/80G  alice                   ⣿⣿⣿⣿⣿⣿⣿   53°  558W │
│ ● 1                            0%                              0.0/80G  -                       ⣀⣀⣀⣀⣀⣀⣀   35°  102W │
│ ──────────────────────────────────────────────────────────────────────────────────────────────────────────────────── │
│     PID  USER    GPU   VRAM   %CPU     RSS    TIME  COMMAND                                                          │
│ 3422397  alice   0    72.1G    101    3.2G  16m47s  python train.py --config big.yaml                                │
│ cpu users  bob 3617% 233.8G · alice 101% 3.2G · root 9% 7.9G                                                        │
╰──────────────────────────────────────────────────────────────────────────────────────────────────── up 50d · ↻ 49ms ─╯
 ← host →  reverse  free  cpu  hist  user  proc  list  ␣ pause                     − 100ms +  theme auto  ? help  quit
```

- **GPU rows**: utilization, VRAM, who uses the card (`+N` = N more users, yours highlighted), utilization history, temperature, power. The dot is green when idle (util and VRAM < 5%), yellow when lightly used (< 40%), red when busy, and fades between them.
- **CPU rows**: total CPU, RAM (used, then page cache in a muted tone), swap, CPU history and package temperature on the GPU columns; below them one cell per core (busiest core per cell when there are more cores than room) and the 1 / 5 / 15 min load.
- **Process list** (`p`): the processes on each GPU with their current %CPU (100 = one core) and host RSS, plus the users keeping the host's CPUs busy right now.
- Title: GPU and CPU models. Bottom edge: uptime and the request round trip.

## Install

macOS and Linux, x86_64 and arm64. You need key-based (password-less) SSH to each server. The servers need only the NVIDIA driver; with python3 (on practically every Linux) rgtop reads the driver's NVML library directly, otherwise it falls back to running `nvidia-smi`.

```bash
curl -fsSL https://raw.githubusercontent.com/xianull/rgtop/main/install.sh | sh
```

This downloads the binary for your platform, verifies its SHA-256 and installs it to `~/.local/bin/rgtop`. Options:

```bash
curl -fsSL https://raw.githubusercontent.com/xianull/rgtop/main/install.sh | RGTOP_INSTALL_DIR=/usr/local/bin sh
curl -fsSL https://raw.githubusercontent.com/xianull/rgtop/main/install.sh | RGTOP_VERSION=v4.0.0 sh
```

Or download `rgtop-<arch>-<os>.tar.gz` from [Releases](https://github.com/xianull/rgtop/releases), or build it: `cargo install --git https://github.com/xianull/rgtop`.

Upgrade by running the installer again; uninstall with `rm ~/.local/bin/rgtop` (settings live in `~/.config/rgtop/`).

> Coming from gpustatus: hosts and settings in `~/.config/gpustatus/` are picked up automatically, and `$GPUSTATUS_HOSTS` / `$GPUSTATUS_THEME` still work.

## Servers

rgtop connects to **Host aliases** from `~/.ssh/config` (see `ssh_config.example`), taken from the first of:

1. arguments: `rgtop gpu-h100-1 gpu-h100-2`
2. `$RGTOP_HOSTS`: `export RGTOP_HOSTS="gpu-h100-1 gpu-h100-2"` (spaces or commas)
3. `~/.config/rgtop/hosts`: one or more aliases per line, `#` starts a comment

A host that can't be reached shows a red error card and retries with backoff; the others are unaffected.

## Usage

```bash
rgtop              # live dashboard
rgtop -1           # print one snapshot and exit (automatic when stdout isn't a terminal)
rgtop -n 0.1       # start with a 100 ms refresh interval
rgtop --reset      # ignore saved settings
```

| Key | Action |
| --- | --- |
| `−` / `+` | refresh interval ±100 ms (from 100 ms), applies instantly |
| `←` / `→` | sort hosts: `host` (config order), `idle`, `vram` (most free first), `util` (least busy first) |
| `r` | reverse the sort |
| `f` | free only: hide busy GPUs |
| `c` `h` `u` `p` | toggle CPU rows, utilization history, USER column, process list |
| `l` | list view: one row per host; click a row to open its card |
| `1`–`9` / `a` | fold host N / all hosts (or click a card title) |
| `space` / `enter` | pause or resume sampling / sample now |
| `↑` `↓` `PgUp` `PgDn` `g` `G` wheel | scroll |
| `t` | theme: auto (from the terminal background), dark, light |
| `m` | mouse capture on/off (off lets you select text) |
| `?` / `q` | help / quit |

Every button in the bottom bar is clickable, like btop. Interval, sort, toggles, view, theme and folded hosts are saved to `~/.config/rgtop/settings.json` on exit.

## How it works

- **One SSH session per host, on demand.** The session runs a small agent that answers one request line with one frame, so the local side decides when to sample: interval changes apply instantly, pausing costs nothing, and no sample pays for a handshake. Lost connections retry after 2 s, 4 s, 8 s … up to 30 s.
- **No forks per sample.** The agent (`src/agent.py`, Python 3 stdlib) reads GPUs through NVML and the host from `/proc` in-process: ~1 ms per sample, versus ~250 ms to start `nvidia-smi`. GPU process lists are refreshed once a second, the per-core strip twice a second, the host's top CPU users every 5 s in a background thread. On a host without python3, a shell agent running `nvidia-smi` takes over.
- **Rendering.** Frames are rebuilt from scratch and diffed cell by cell against the previous one; only the changed span of each row is sent, inside a synchronized update (no tearing). It animates at 60 fps while something moves (bars, numbers and status dots ease) and sleeps otherwise.
- **Measured** with 5 servers × 8 GPUs: at a 2 s interval rgtop uses 0.8% of one local core and the agent 0.8% of one server core; at 100 ms, 2.8% locally and 2.6% on the server (of 192 cores). Memory: 4 MiB.
- **Details.** `ClearAllForwardings=yes`, so port forwards in `~/.ssh/config` aren't triggered; output of remote shell startup files is ignored and control characters from remote data never reach your terminal. CPU usage comes from `/proc/stat` deltas, RAM used is `MemTotal − MemAvailable`. A host with no fresh data for `max(3 × interval, interval + 8 s)` is marked `stale`.

## Development

```bash
cargo run -- host1 host2                               # run
cargo test                                             # tests
cargo test --release -- --ignored bench_frame --nocapture   # frame-time benchmark
git tag v4.0.1 && git push --tags                      # CI builds 4 platforms and publishes a release
```

## License

MIT
