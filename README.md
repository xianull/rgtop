# <picture><source media="(prefers-color-scheme: dark)" srcset="docs/logo-dark.svg"><source media="(prefers-color-scheme: light)" srcset="docs/logo-light.svg"><img alt="rgtop" src="docs/logo-dark.svg" width="430"></picture>

[![Release](https://img.shields.io/github/v/release/xianull/rgtop)](https://github.com/xianull/rgtop/releases/latest)
[![CI](https://img.shields.io/github/actions/workflow/status/xianull/rgtop/release.yml?branch=main&label=CI)](https://github.com/xianull/rgtop/actions/workflows/release.yml)
[![Downloads](https://img.shields.io/github/downloads/xianull/rgtop/total)](https://github.com/xianull/rgtop/releases)
[![License](https://img.shields.io/github/license/xianull/rgtop)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-informational)](#install)
[![Rust](https://img.shields.io/badge/rust-1.90%2B-orange?logo=rust)](https://www.rust-lang.org)

**English** | [简体中文](README.zh-CN.md)

**rgtop** (**r**emote **G**PU **top**) is a btop-style live dashboard for NVIDIA GPUs, CPUs and memory on many remote servers, over plain SSH. One ~700 KB static binary on your machine; nothing to install on the servers.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/screenshot-dark.png">
  <source media="(prefers-color-scheme: light)" srcset="docs/screenshot-light.png">
  <img alt="rgtop: four GPU servers, two expanded with CPU rows, GPU rows and processes, two folded" src="docs/screenshot-dark.png">
</picture>

- **GPU rows**: utilization, VRAM, who uses the card (`+N` = N more users, yours highlighted), utilization history, temperature, power. The dot is green when idle (util and VRAM < 5%), yellow when lightly used (< 40%), red when busy, and fades between them.
- **CPU rows**: total CPU, RAM (used, then page cache in a muted tone), 1-minute load, CPU history and package temperature on the GPU columns; below them (toggle with `o`) one cell per core (busiest core per cell when there are more cores than room), the 1 / 5 / 15 min load and swap.
- **Process list** (`p`): the processes on each GPU with their current %CPU (100 = one core) and host RSS, plus the users keeping the host's CPUs busy right now.
- Title: GPU and CPU models. Bottom edge: uptime and the request round trip.

## Install

macOS and Linux, x86_64 and arm64. Key-based SSH to each server is smoothest; servers that ask for a password work too (see [Password login](#password-login)). The servers need only the NVIDIA driver; with python3 (on practically every Linux) rgtop reads the driver's NVML library directly, otherwise it falls back to running `nvidia-smi`.

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

## Password login

Servers that want a password, a one-time code or a new host key confirmed work too. rgtop steps out of the full screen and `ssh` itself asks: rgtop never sees or stores the password. `ssh` then keeps the login open in the background (OpenSSH connection sharing, socket `~/.ssh/rgtop-<host>`) for as long as rgtop uses it and 8 hours after, so reconnects and new rgtop sessions don't ask again. A network change or a sleeping laptop can still end it; the card then asks you to press `L`.

- rgtop asks once at startup for each such host; `Ctrl-C` at the prompt skips that host.
- When a login has expired, the card says so: press `L` (or click `L login` in the bottom bar) to log in again.
- To end a login early: `ssh -O exit -o ControlPath=~/.ssh/rgtop-<host> <host>`.

Right after a password login rgtop offers to put your SSH key on that server, over the login you just made (like `ssh-copy-id`): press `y` and the host doesn't ask again. It installs the first key file `ssh` tries for that host (shown in full before you answer; no key yet? `ssh-keygen -t ed25519`) and checks that it works: servers that insist on a password or one-time code keep asking, and rgtop says so. Mind what it means: whoever has that key's private half can then log in there without the password or code. Keep a passphrase on the key (ssh-agent remembers it), and check your site's rules if it requires one-time codes.

## Environment variables

| Variable | Effect |
| --- | --- |
| `RGTOP_HOSTS` | hosts to show when none are given as arguments: SSH aliases, separated by spaces or commas |
| `RGTOP_THEME` | `auto`, `dark` or `light`: the theme until settings are first saved; after that the saved theme wins (switch it with `t`, or override with `--theme`) |
| `XDG_CONFIG_HOME` | config location: `$XDG_CONFIG_HOME/rgtop/` (default `~/.config/rgtop/`) |
| `NO_COLOR` | set to anything: no colours |
| `COLORTERM` | `truecolor` or `24bit` turns on 24-bit colour (most modern terminals set it); otherwise 256 colours are used |
| `COLUMNS` | width of `rgtop -1` output when it doesn't go to a terminal (default 80) |
| `RGTOP_INSTALL_DIR`, `RGTOP_VERSION` | used by `install.sh`, see [Install](#install) |

`GPUSTATUS_HOSTS` and `GPUSTATUS_THEME` still work when the `RGTOP_` variables aren't set.

To set one for a single run, put it in front of the command; to keep it, add an `export` line to your shell's startup file:

```bash
# this run only
RGTOP_HOSTS="gpu-h100-1 gpu-h100-2" rgtop

# zsh (the macOS default): ~/.zshrc; bash: ~/.bashrc
echo 'export RGTOP_HOSTS="gpu-h100-1 gpu-h100-2"' >> ~/.zshrc
source ~/.zshrc          # apply it to the shell you're in; new terminals get it anyway

# fish
set -Ux RGTOP_HOSTS "gpu-h100-1 gpu-h100-2"
```

Check it with `echo $RGTOP_HOSTS`. If your shell says `rgtop: command not found` after installing, add the install directory to `PATH` the same way: `echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc`.

## Usage

```bash
rgtop              # live dashboard
rgtop -1           # print one snapshot and exit (automatic when stdout isn't a terminal)
rgtop -n 0.1       # start with a 100 ms refresh interval
rgtop -n auto      # as fast as each host answers (up to 20 samples/s)
rgtop --reset      # ignore saved settings
```

| Key | Action |
| --- | --- |
| `−` / `+` | refresh interval ±100 ms, applies instantly; below 100 ms is `auto`: every host is asked again as soon as it answered (up to 20/s) |
| `←` / `→` | sort hosts: `host` (config order), `idle`, `vram` (most free first), `util` (least busy first) |
| `r` | reverse the sort |
| `f` | free only: hide busy GPUs |
| `c` `o` `h` `u` `p` | toggle CPU rows, per-core strip, utilization history, USER column, process list |
| `l` | list view: one row per host; click a row to open its card |
| `1`–`9` / `a` | fold host N / all hosts (or click a card title) |
| `space` / `enter` | pause or resume sampling / sample now |
| `↑` `↓` `PgUp` `PgDn` `g` `G` wheel | scroll |
| `t` | theme: auto (from the terminal background), dark, light |
| `m` | mouse capture on/off (off lets you select text) |
| `L` | log in to hosts that want a password, see [Password login](#password-login) |
| `?` / `q` | help / quit |

Every button in the bottom bar is clickable, like btop. Interval, sort, toggles, view, theme and folded hosts are saved to `~/.config/rgtop/settings.json` on exit.

### Themes

`auto` picks dark or light from your terminal's background; `t` cycles auto → dark → light.

| Dark | Light |
| --- | --- |
| <img alt="rgtop, dark theme" src="docs/screenshot-dark.png"> | <img alt="rgtop, light theme" src="docs/screenshot-light.png"> |

## How it works

- **One SSH session per host, on demand.** The session runs a small agent that answers one request line with one frame, so the local side decides when to sample: interval changes apply instantly, pausing costs nothing, and no sample pays for a handshake. Lost connections retry after 2 s, 4 s, 8 s … up to 30 s.
- **No forks per sample.** The agent (`src/agent.py`, Python 3 stdlib) reads GPUs through NVML and the host from `/proc` in-process: ~1 ms per sample, versus ~250 ms to start `nvidia-smi`. GPU process lists are refreshed once a second, the per-core strip up to five times a second, the host's top CPU users every 5 s in a background thread. On a host without python3, a shell agent running `nvidia-smi` takes over.
- **Rendering.** Frames are rebuilt from scratch and diffed cell by cell against the previous one; only the changed cells are sent (unchanged stretches in between are skipped with a cursor move), inside a synchronized update (no tearing). Bars, numbers and status dots ease at up to 30 fps for ~0.3 s after new data, then the screen is left alone: every frame is work for your terminal and the window server too.
- **Measured** with 5 servers × 8 GPUs: at a 2 s interval rgtop uses 0.4% of one local core and ~6 terminal redraws/s, the agent 0.8% of one server core; at 100 ms, 1.9% locally (28 redraws/s) and 2.6% on the server (of 192 cores). Memory: 4 MiB.
- **No leftovers.** rgtop pings every agent at least every 15 s, paused or not, and an agent that hears nothing for 60 s exits: a connection whose close never reached the server (proxies, VPNs, a sleeping laptop) can't leave it running.
- **Details.** `ClearAllForwardings=yes`, so port forwards in `~/.ssh/config` aren't triggered; output of remote shell startup files is ignored and control characters from remote data never reach your terminal. CPU usage comes from `/proc/stat` deltas, RAM used is `MemTotal − MemAvailable`. A host with no fresh data for `max(3 × interval, interval + 8 s)` is marked `stale`.

## Development

```bash
cargo run -- host1 host2                               # run
cargo test                                             # tests
cargo test --release -- --ignored bench_frame --nocapture   # frame-time benchmark
uv run --with pyte --with pillow --with fonttools docs/screenshots.py   # README screenshots (made-up hosts)
git tag v4.1.1 && git push --tags                      # CI builds 4 platforms and publishes a release
```

## License

MIT
