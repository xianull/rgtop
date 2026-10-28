# rgtop

[English](README.md) | **简体中文**

btop 风格的多机 GPU / CPU / 内存实时看板。在本地运行，通过 SSH 连接各台服务器，键盘和鼠标都能操作。本地只有一个约 700 KB 的静态二进制，服务器上什么都不用装。

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

- **GPU 行**：利用率、显存、谁在用这张卡（`+N` 表示还有 N 个用户，你自己会高亮）、利用率历史、温度、功耗。圆点绿色是空闲（利用率和显存都 < 5%），黄色是轻度占用（都 < 40%），红色是繁忙；状态变化时颜色会渐变过去。
- **CPU 行**：和 GPU 列对齐，依次是 CPU 总占用、内存（已用部分用渐变色，page cache 用暗色接在后面）、1 分钟负载、CPU 历史曲线、CPU 封装温度。下面一行（按 `o` 开关）是每核热力条（核比格子多时，每格显示组里最忙的那个核），右边是 1 / 5 / 15 分钟负载和 swap。
- **进程列表**（`p`）：每张卡上的进程，带**当前** %CPU（100 = 一个核）和主机内存 RSS；最后一行是此刻最占 CPU 的用户，不管是不是 GPU 任务，方便找出谁在把 CPU 跑满。
- 标题显示 GPU 和 CPU 型号；底边显示开机时长和一次采样的往返耗时。

## 安装

支持 macOS 和 Linux（x86_64 / arm64）。本机需要能用 SSH **免密**（密钥）登录各台服务器。服务器上只要装了 NVIDIA 驱动即可：有 python3（几乎所有 Linux 都有）时，rgtop 直接调用驱动自带的 NVML 库；没有时退回到运行 `nvidia-smi`。

```bash
curl -fsSL https://raw.githubusercontent.com/xianull/rgtop/main/install.sh | sh
```

脚本会下载对应平台的二进制、校验 SHA-256，装到 `~/.local/bin/rgtop`。可选：

```bash
curl -fsSL https://raw.githubusercontent.com/xianull/rgtop/main/install.sh | RGTOP_INSTALL_DIR=/usr/local/bin sh
curl -fsSL https://raw.githubusercontent.com/xianull/rgtop/main/install.sh | RGTOP_VERSION=v4.0.0 sh
```

也可以从 [Releases](https://github.com/xianull/rgtop/releases) 手动下载 `rgtop-<arch>-<os>.tar.gz`，或者用 Rust 编译：`cargo install --git https://github.com/xianull/rgtop`。

升级就是重新跑一遍安装命令；卸载：`rm ~/.local/bin/rgtop`（设置在 `~/.config/rgtop/`）。

> 从旧版 gpustatus 升级：`~/.config/gpustatus/` 里的 hosts 和设置会被自动读取，环境变量 `GPUSTATUS_HOSTS` / `GPUSTATUS_THEME` 也仍然有效。

## 配置服务器

按 `~/.ssh/config` 里的 **Host 别名**连接（写法参考 `ssh_config.example`），按以下顺序取第一个非空的来源：

1. 命令行参数：`rgtop gpu-h100-1 gpu-h100-2`
2. 环境变量：`export RGTOP_HOSTS="gpu-h100-1 gpu-h100-2"`（空格或逗号分隔）
3. `~/.config/rgtop/hosts`：每行一个或多个别名，`#` 后是注释

连不上的机器只显示一个红色错误卡片并按退避间隔重连，不影响其他机器。

## 使用

```bash
rgtop              # 实时看板（全屏）
rgtop -1           # 打印一次快照就退出；输出不是终端时自动这样
rgtop -n 0.1       # 以 100ms 刷新间隔启动
rgtop -n auto      # 每台机器一回数据就立刻再采（最多每秒 20 次）
rgtop --reset      # 忽略保存的设置
```

| 按键 | 作用 |
| --- | --- |
| `−` / `+` | 刷新间隔 ±100ms，立即生效；100ms 再往下是 `auto`：每台机器一回数据就立刻再采（最多每秒 20 次） |
| `←` / `→` | 主机排序：`host`（配置顺序）、`idle`（空闲卡多的在前）、`vram`（剩余显存多的在前）、`util`（最闲的在前） |
| `r` | 反转排序 |
| `f` | 只看空闲：隐藏繁忙的卡 |
| `c` `o` `h` `u` `p` | 开关 CPU 行、每核热力条、利用率历史、USER 列、进程列表 |
| `l` | 列表视图：每台机器一行；点某一行跳到该机器的卡片 |
| `1`–`9` / `a` | 折叠第 N 台 / 全部机器（也可以点卡片标题） |
| `空格` / `回车` | 暂停或恢复采样 / 立即采样一次 |
| `↑` `↓` `PgUp` `PgDn` `g` `G` 滚轮 | 滚动 |
| `t` | 配色：auto（按终端背景自动选）、dark、light |
| `m` | 开关鼠标捕获（关掉后可以选中复制文字） |
| `?` / `q` | 帮助 / 退出 |

底栏和 btop 一样，每个按钮都能用鼠标点。刷新间隔、排序、各个开关、视图、配色、折叠了哪些机器，退出时保存到 `~/.config/rgtop/settings.json`。

## 实现要点

- **每台机器一条 SSH 长连接，按需采样**：远端运行一个小 agent，本地每发一行请求，远端回一帧数据。所以改刷新间隔立即生效，暂停时完全不采样，任何一次采样都不用重新握手。断线后按 2s、4s、8s… 退避重连，最长 30 秒。
- **采样时远端零 fork**：agent（`src/agent.py`，只用 Python 3 标准库）在进程内通过 NVML 读 GPU、读 `/proc` 拿主机数据，一次采样约 1ms，而启动一次 `nvidia-smi` 要约 250ms。GPU 进程列表每秒刷新一次，每核热力条每 0.5 秒一次，全机 CPU 用户统计在后台线程里每 5 秒一次。服务器没有 python3 时，改用运行 `nvidia-smi` 的 shell 版 agent。
- **渲染**：每帧整体重建，再和上一帧逐格比较，每行只发送变化的那一段，并用同步输出（DEC 2026）避免撕裂。新数据到来后，条形图、数字、状态圆点以最多 30fps 缓动约 0.3 秒，之后画面静止：每一帧都要终端和系统窗口服务器重绘，帧数越少电脑越不卡。
- **实测**（5 台服务器 × 8 卡）：2 秒间隔时，rgtop 本地占单核 0.4%、终端每秒重绘约 6 次，远端 agent 占单核 0.8%；100ms 间隔时，本地 1.9%（每秒重绘 28 次），远端 2.6%（服务器共 192 核）。内存 4 MiB。
- **不留残余进程**：rgtop 至少每 15 秒给每个 agent 发一次心跳（暂停时也发），agent 60 秒收不到任何东西就自动退出。即使断开时的关闭包没送到服务器（代理、VPN、电脑休眠），也不会在服务器上留下进程。
- **其他**：带了 `ClearAllForwardings=yes`，不会触发 `~/.ssh/config` 里配的端口转发；远端 shell 启动文件打印的内容会被忽略，远端数据里的控制字符不会原样输出到终端。CPU 占用由 `/proc/stat` 差值计算，内存已用 = `MemTotal − MemAvailable`。超过 `max(3 × 间隔, 间隔 + 8 秒)` 没有新数据的机器标记为 `stale`。

## 开发

```bash
cargo run -- host1 host2                                    # 调试运行
cargo test                                                  # 单元测试
cargo test --release -- --ignored bench_frame --nocapture   # 单帧耗时基准测试
git tag v4.0.1 && git push --tags                           # CI 编译 4 个平台并发布 Release
```

## License

MIT
