# gpustatus

btop 风格的多机 GPU / CPU / 内存实时看板。在本地运行，通过 SSH 并发连接各台服务器，键盘和鼠标都能操作。单个 ~800 KB 的静态二进制，无运行时依赖。

*A btop-style live dashboard for NVIDIA GPUs on many remote servers, over plain SSH. One static binary, nothing to install on the servers.*

```
 ◆ gpustatus   6/40 GPUs idle  ·  1,577 GiB VRAM free  ·  5/5 hosts  ·  you on 6 GPUs        ● idle  ● light  ● busy    15:24:39
╭─ ▾ ● gpu-h100-1  8× H100 80GB HBM3 ──────────────────────────────────────── 4/8 idle · 505G free ─╮
│ CPU █▉                 11%  RAM █▊             274/2016G                           load 30.1/192c │
│   # UTIL                    VRAM                          USER        HISTORY          TEMP POWER │
│ ● 0                     0%  ██▌                 12.0/80G  alice+1                   ⣀   24°  116W │
│ ● 7 █████████████████ 100%  ████                18.6/80G  bob                       ⢸   45°  475W │
╰───────────────────────────────────────────────────────────────────────────────────────── ↻ 508ms ─╯
 ← host →  reverse  free  cpu  hist  user  proc  list  ␣ pause                   − 2000ms +  theme auto  ? help  quit
```

- 每张卡前的圆点：绿色空闲（util < 5% 且显存 < 5%），黄色轻度占用（都 < 40%），红色繁忙；状态变化时颜色会渐变过去
- USER：卡上占显存最多的用户，`+N` 表示还有 N 个用户。你自己的用户名会高亮
- HISTORY：GPU 利用率的历史曲线
- 卡片右下角的 `↻ 508ms`：这台机器一次采样的往返耗时
- 顶栏：所有机器的空闲卡数、剩余显存、在线机器数，以及你自己占了几张卡

## 安装

支持 macOS 和 Linux（x86_64 / arm64）。只需要本机能用 SSH **免密**（密钥）登录各台服务器；服务器上只要有 `nvidia-smi` 和 `sh`，什么都不用装。

```bash
curl -fsSL https://raw.githubusercontent.com/xianull/gpustatus/main/install.sh | sh
```

会下载对应平台的二进制、校验 SHA256，装到 `~/.local/bin/gpustatus`。可以用环境变量改：

```bash
curl -fsSL https://raw.githubusercontent.com/xianull/gpustatus/main/install.sh | GPUSTATUS_INSTALL_DIR=/usr/local/bin sh
curl -fsSL https://raw.githubusercontent.com/xianull/gpustatus/main/install.sh | GPUSTATUS_VERSION=v3.0.0 sh
```

其他方式：

- 从 [Releases](https://github.com/xianull/gpustatus/releases) 手动下载 `gpustatus-<arch>-<os>.tar.gz`，解压后放进 `PATH`
- 有 Rust 工具链：`cargo install --git https://github.com/xianull/gpustatus`

升级就是重新跑一遍安装命令；卸载：`rm ~/.local/bin/gpustatus`（设置在 `~/.config/gpustatus/`）。

## 配置服务器

按 `~/.ssh/config` 里的 **Host 别名**连接服务器，按以下顺序取第一个非空的来源：

1. 命令行参数：`gpustatus gpu-h100-1 gpu-h100-2`
2. 环境变量：`export GPUSTATUS_HOSTS="gpu-h100-1 gpu-h100-2"`（空格或逗号分隔）
3. `~/.config/gpustatus/hosts`：每行一个或多个别名，`#` 后是注释

别名的写法参考 `ssh_config.example`。连不上的机器只会显示一个红色错误卡片，不影响其他机器。

## 使用

```bash
gpustatus              # 实时看板（全屏）
gpustatus -1           # 打印一次快照就退出；输出不是终端时自动这样
gpustatus -n 1         # 以 1 秒刷新间隔启动
gpustatus --reset      # 忽略保存的设置，从默认值开始
```

### 按键与鼠标

底栏和 btop 一样，每个按钮都能用鼠标点，也可以按里面高亮的那个字母。

| 按键 | 作用 |
| --- | --- |
| `−` / `+` | 调刷新间隔，可选 0.5s、1s、1.5s、2s、3s、5s、10s、30s、60s，立即生效 |
| `←` / `→` | 切换主机排序：`host`（配置顺序）、`idle`（空闲卡多的在前）、`vram`（剩余显存多的在前）、`util`（最闲的在前） |
| `r` | 反转排序 |
| `f` | 只看空闲：隐藏繁忙的卡 |
| `c` `h` `u` `p` | 开关 CPU 行、利用率历史、USER 列、进程列表（PID、用户、GPU、显存、运行时长、命令） |
| `l` | 列表视图：每台机器一行，适合机器多的时候一屏看完；点某一行会跳到该机器的卡片 |
| `1`–`9` / `a` | 折叠或展开第 N 台机器 / 全部机器。点卡片标题也能折叠 |
| `空格` / `回车` | 暂停或恢复采样 / 立即采样一次 |
| `↑` `↓` `PgUp` `PgDn` `g` `G` 滚轮 | 内容超过一屏时滚动 |
| `t` | 切换配色：auto（按终端背景自动选）、dark、light |
| `m` | 开关鼠标捕获。关掉后可以用鼠标选中、复制文字 |
| `?` | 帮助 |
| `q` | 退出 |

刷新间隔、排序、各个开关、视图、配色、折叠了哪些机器，退出时都会保存到 `~/.config/gpustatus/settings.json`，下次启动沿用，和 btop 保存 `btop.conf` 一样。

## 实现要点

- **按需采样**：每台机器只建一条 SSH 长连接，远端运行一个小 sh 循环。本地每发一行请求，远端回一帧数据，所以刷新间隔改了立即生效，暂停时完全不采样，任何一次采样都不用重新握手。断线后按 2s、4s、8s… 退避重连，最长间隔 30 秒。
- **进程信息**：只有在打开 USER 列或进程列表时才查询（`nvidia-smi --query-compute-apps` 加 `ps`）。在宿主机上看不到的 PID（其他容器里的，或者已经退出但显存没释放的），会合并成一行 `?` 显示。
- **SSH 选项**：带了 `ClearAllForwardings=yes`，不会触发 `~/.ssh/config` 里配的端口转发。远端 shell 启动文件打印的内容会被忽略；远端数据里的控制字符不会原样输出到终端。
- **数据口径**：CPU 占用由 `/proc/stat` 两次采样的差值计算；RAM 已用量是 `MemTotal - MemAvailable`。
- **动画**：有东西在动时以 60 fps 渲染，否则休眠到下一个采样点或时钟的下一秒。条形图、百分比和显存数字一起做指数缓动；状态圆点在绿 / 黄 / 红之间渐变；每次采样后主机圆点亮一下再淡出。
- **渲染**：每帧整体重建（几十微秒），再和上一帧逐格比较，每行只发送真正变化的那一段，并使用同步输出（DEC 2026），不撕裂。实测 5 台机器、40 张卡、2 秒间隔时：CPU 约为单核的 2.5%，写给终端约 26 KiB/s，内存 4 MiB（Python 旧版分别是 4%、108 KiB/s、31 MiB）。
- **超时与过期**：连接加首次采样超过 `--timeout + 7` 秒，判定为超时。超过 `max(3 × 间隔, 间隔 + 8)` 秒没收到新数据的机器，会标记为 `stale`。

## 开发

```bash
cargo run -- host1 host2     # 调试运行
cargo test                   # 单元测试
git tag v3.0.1 && git push --tags   # CI 编译 4 个平台并发布 Release
```

## License

MIT
