# gpuwho

[![CI](https://github.com/htyvv/gpuwho/actions/workflows/ci.yml/badge.svg)](https://github.com/htyvv/gpuwho/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/htyvv/gpuwho)](https://github.com/htyvv/gpuwho/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-green)](LICENSE)

**实验室所有 GPU 服务器的 nvidia-smi：一个终端看清每张卡谁在用、哪张空着。**

走 SSH，服务器上什么都不用装：gpuwho 通过 SSH 拷过去一个 600 KB 的小程序（或者用 `python3` 跑一个 Python 版），直接读 NVML。

[English](README.md) | 简体中文 | [한국어](README.ko.md)

<p align="center">
  <img src="docs/demo.gif" alt="gpuwho 实时界面：两台服务器、16 张卡，任务结束又开始，每张卡是谁在用" width="100%">
  <br>
  <sub>画面里的服务器和用户来自 <code>gpuwho demo</code>，都是编的：node01 上一个任务跑完，node03 上又开了一个。</sub>
</p>

```sh
curl -fsSL https://raw.githubusercontent.com/htyvv/gpuwho/main/install.sh | sh
gpuwho demo                   # 先试试，不需要服务器
gpuwho add gpu1 10.0.0.11     # 再加上自己的
gpuwho
```

## 以前 / 现在

| 以前 | 现在 |
|---|---|
| 挨个 `ssh` 上去跑 `nvidia-smi` | 所有服务器、所有卡，一屏看完，每 3 秒刷新 |
| `nvidia-smi` 只给 PID，再拿去 `ps` 里查是谁 | 直接显示用户名和占了多少显存 |
| Docker 里的任务全是 root，不知道是谁的 | 显示为 `root@3f9c2a`（容器 ID），能对上是哪个容器 |
| 在群里喊一声"谁在用 3 号卡""谁占着卡不跑" | `free` 列出空卡，`claim` 登记占用，占着不跑的卡标出来 |
| 写脚本循环等空卡 | `gpuwho free -n 2 --wait -- python train.py` |

## 能看到什么

- 每台服务器每张卡的利用率、显存、温度、风扇、功耗；在用编解码器时也会显示。
- 每张卡是谁在用、占了多少显存。Docker 里的任务显示为容器 ID，比如 `root@3f9c2a`。
- 哪些卡现在空着：没有进程、显存占用 < 1 GiB、利用率 < 5% 才算空闲。只挂着 Xorg、gnome-shell 的卡也算空。
- 谁占着卡不用：占着 ≥ 1 GiB、30 分钟基本 0% 的任务会标上 `idle 2h`，按人汇总里多一列 *Held idle*。
- 两个任务挤在同一张卡上时，看得出谁在真正计算（SM%），谁只是占着显存。
- 按人汇总：每个人在所有服务器上占了几张卡、多少显存、在哪几台（如 `node01[0,1]`），以及 CPU 核数和内存。内存按内核记在这个人 cgroup 上的量算，DataLoader fork 出来的 worker 不会被重复算 30 遍。
- 不属于任何进程的显存、看起来像挖矿的进程、超过组里卡数上限的人（`--max-gpus 2`）、掉卡、MIG 实例。

终端够宽时服务器并排显示；服务器多到放不下时自动切成每台一行（按 `c` 切换），还放不下就滚动。

<p align="center">
  <img src="docs/screenshot.png" alt="四台服务器并排，包括一台纯 CPU 机器和一张掉了的卡" width="100%">
</p>

## 组里一起用

"谁在用"之后的事：等卡、占卡、收拾。

```sh
gpuwho free                                   # 所有空闲的卡，剩余显存多的在前
gpuwho free gpu1 -n 2 --wait -- python train.py
                                              # 在 gpu1 上：等到有 2 张空卡，设好 CUDA_VISIBLE_DEVICES 再运行
gpuwho free -n 4 --wait --notify https://open.feishu.cn/open-apis/bot/v2/hook/...
                                              # 飞书、企业微信、钉钉、Slack、Discord、ntfy、Telegram 都行
gpuwho claim gpu1:2,3 --for 6h --note "赶 CVPR"
gpuwho claims                                 # 大家都看得到；`free` 会跳过别人登记的卡
gpuwho unclaim gpu1
gpuwho kill gpu1:3                            # 只动你自己在那张卡上的进程，先列出来再问你
```

登记只是告诉大家，不是锁：和预约表一样，不强制。登记存在每台服务器的 `/var/tmp/gpuwho-claims` 里，以文件属主为准，别人没法冒用你的名字。

```sh
gpuwho record usage.jsonl            # 每分钟记一次；放在 tmux 里，或者 cron 里用 `--once`
gpuwho report usage.jsonl --since 7d # 每个人的卡时，以及占着不用的时间
gpuwho once --html > ~/public_html/gpus.html   # cron 生成一个组里都能看的页面
```

还支持 MCP，Claude Code、Cursor 这类编程智能体启动任务前可以先查空卡：

```sh
claude mcp add gpuwho -- gpuwho mcp   # 工具：gpu_status、free_gpus、who_uses
```

## 安装

```sh
curl -fsSL https://raw.githubusercontent.com/htyvv/gpuwho/main/install.sh | sh
```

会在 `~/.local/bin` 放一个可执行文件（Linux x86_64/aarch64、macOS）。Windows 在 [Releases](https://github.com/htyvv/gpuwho/releases) 下载 `gpuwho-x86_64-pc-windows-msvc.zip`。也可以自己编译：`cargo install --git https://github.com/htyvv/gpuwho gpuwho`。

访问 GitHub 慢的话，在 Releases 页面下载对应的压缩包，解压后把 `gpuwho` 放进 PATH 就行，校验值在 `SHA256SUMS` 里。

服务器那边只需要 Linux 和 NVIDIA 驱动。不需要 root，没有守护进程，不开端口，不用找管理员。组里服务器共享 home（NFS）的话，装一次哪台节点都能用。

## 用法

每台服务器加一次。gpuwho 会先连一下确认能用，再保存。

```sh
gpuwho add gpu1 10.0.0.11                  # 用你的 SSH 密钥或 ssh-agent
gpuwho add gpu2 alice@10.0.0.12 --password # 输入密码并保存（文件权限 0600）
gpuwho add dgx dgx.example.org:2222 --key ~/.ssh/id_lab
gpuwho add lab-a100                        # ~/.ssh/config 里的 Host，跳板机（ProxyJump）也照用
gpuwho add here local                      # 本机，不走 SSH
```

没写的（用户名、端口、密钥、跳板机）都按 `ssh` 的规则从 `~/.ssh/config` 里取。

然后运行 `gpuwho`：

| 按键 | |
|---|---|
| `q` | 退出 |
| `p` | 进程表（PID、用户、显存、SM%、CPU%、内存、运行时长、命令行） |
| `c` | 紧凑视图，每台服务器一行 |
| `j` `k` `↑` `↓` `PgUp` `PgDn` `g` `G` | 滚动 |
| `r` | 立刻重连失败的服务器 |
| `space` | 暂停 |
| `?` | 帮助 |

```
gpuwho                       实时面板，每 3 秒刷新
gpuwho -i 1 -p               每秒刷新，带进程表
gpuwho once                  打印一次就退出（在 watch、cron、管道里运行也是这样）
gpuwho once --json           同上，JSON 格式（字段固定）
gpuwho test                  检查所有连接，有一台失败就返回 1
gpuwho list | remove gpu2
```

## 原理

- 每台服务器一个 SSH 连接（[russh](https://github.com/Eugeny/russh)）和一个常驻的小进程。每次刷新发一个换行，收回一行 JSON，不用每次重新握手、重新起进程。
- 这个小进程是按 glibc 2.17 编译的 600 KB Rust 程序（CentOS 7 及以上，x86_64 和 aarch64）。只拷一次，放在你的运行时目录（`/run/user/UID`，tmpfs，退出登录就没了）或 `~/.cache/gpuwho`，之后一直复用。跑不了的服务器（noexec、其他架构）改用 `python3 -c` 运行 [`probe.py`](crates/gpuwho/assets/probe.py)：同样的 JSON，不落盘，Python 3.6 起就行。
- 显卡数据通过 `dlopen` 直接读 NVML（`nvidia-smi` 背后的库），不调用也不解析 `nvidia-smi`。
- 进程属于谁从 `/proc` 读，容器从 `/proc/PID/cgroup` 读（Docker、containerd、Podman、CRI-O）。
- 以你自己的身份运行；SSH 断开、90 秒没有请求或者运行满 6 小时会自己退出。
- NVIDIA 驱动卡死时，NVML 调用 15 秒后放弃，那台机器的 CPU 和内存照常显示。
- 连不上的服务器按 2、4、8 …… 最多 60 秒的间隔重试。密码错误或主机密钥变了不会自动重试，等你按 `r`，不会触发服务器的 fail2ban。

开销（在我们的服务器上测的）：

| | |
|---|---|
| 服务器端，每次刷新（1,750 个进程、8 张卡） | 31 ms CPU（3 秒间隔约占一个核的 1%） |
| 客户端，6 台服务器实时界面 | 一个核的 0.3%，内存 10 MB，3 个线程 |
| 画一帧 64 台服务器 | 21 ms |
| 6 台服务器 `gpuwho test` | 2.3 秒，CPU 14 ms |

gpuwho 0.x 是 Python 写的。用 Rust 重写后，服务器端每次刷新从 47 ms 降到 31 ms，客户端从一个核的 5% 降到 0.3%，64 台服务器的一帧从 412 ms 降到 21 ms。

## 为什么写这个

我们实验室有好几台 GPU 服务器，共享一个 home。想找张空卡，就得挨个 `ssh` 上去跑 `nvidia-smi`，再拿 PID 812345 去 `ps` 里查是谁，然后在群里问谁用完了。gpuwho 把前面这些每 3 秒替你做一遍，群里那部分也给了点结构。

## 和其他工具比

好用的 GPU 监控工具已经不少。下面是 2026 年 10 月看了各项目 README 和源码后整理的区别。

| | 多台服务器一屏 | 每台服务器上需要 | 显示 GPU 进程属于谁 | 界面 |
|---|---|---|---|---|
| **gpuwho** | 是 | 不需要（通过 SSH 拷过去，或用 `python3`） | 是，包括 Docker 任务 | 终端 |
| [nvitop](https://github.com/XuehaiPan/nvitop) | 否，一次一台（`ssh -t host nvitop`） | nvitop | 是 | 终端 |
| [nvtop](https://github.com/Syllo/nvtop) | 否，一次一台 | nvtop | 是 | 终端 |
| [gpustat](https://github.com/wookayin/gpustat) | 否，一次一台 | gpustat | 是 | 终端 |
| [svtop](https://pypi.org/project/svtop/) | 是 | 不需要（运行 `nvidia-smi`） | 是 | 终端 |
| [gpustat-web](https://github.com/wookayin/gpustat-web) | 是 | gpustat | 是 | 浏览器 |
| [all-smi](https://github.com/lablup/all-smi) | 是 | SSH 模式不需要，完整数据要 `all-smi api` | SSH 模式下不显示 | 终端 |
| [cluster-smi](https://github.com/PatWie/cluster-smi) | 是 | 节点守护进程 | 是 | 终端 |
| [gpuview](https://github.com/fgaim/gpuview) | 是 | gpuview 服务 | 部分 | 浏览器 |
| [DCGM exporter](https://github.com/NVIDIA/dcgm-exporter) + Grafana | 是 | exporter、Prometheus | 只有 Kubernetes pod | 浏览器 |

这些情况用别的更合适：

- 只在一台机器上干活，要进程树、历史曲线、Python API，用 **nvitop**。
- 想在监控旁边传文件，用 **svtop**。
- 有 AMD、Intel、Apple 或其他加速卡，用 **nvtop** 或 **all-smi**。gpuwho 只支持 NVIDIA。
- 节点特别多，要 NVLink 拓扑或录制回放，用 **all-smi**。
- 要告警、长期历史或 Kubernetes，用 **DCGM exporter** 配 Prometheus、Grafana，或者 Netdata。
- 想要网页或桌面状态栏，用 **gpustat-web**、**gpuview** 或 [Multiple-smi](https://github.com/ClementPinard/Multiple-smi)（或者 cron 跑 `gpuwho once --html`）。

如果 gpuwho 帮你少 ssh 了几次，点个 star 能让更多实验室找到它。

## 常见问题

**支持 AMD 或 Intel 显卡吗？**
不支持，只支持 NVIDIA（读的是 NVML）。没有 NVIDIA 卡的服务器照样显示 CPU、内存和用户。

**会读 `~/.ssh/config` 吗？跳板机呢？**
会。gpuwho 用 `ssh -G` 取最终配置，所以 `Include`、`Match`、`ProxyJump`、`ProxyCommand` 都能用。跳板机由你本机的 `ssh` 用密钥或 agent 连接，保存的密码只发给最终那台服务器。

**保存密码安全吗？**
密码以明文存在 `~/.config/gpuwho/servers.json`，gpuwho 每次启动都会把权限改回 0600。能用密钥就用密钥。详见 [SECURITY.md](SECURITY.md)。

**会在服务器上留下什么？**
`/run/user/UID`（退出登录就没了）或 `~/.cache/gpuwho` 里的那个小程序，以及用了 `claim` 时 `/var/tmp/gpuwho-claims` 里你的登记文件。在 `servers.json` 里给那台服务器加 `"agent": "python"`（或 `gpuwho add --agent python`），就什么都不写。

**Windows 能用吗？**
能，Windows 版直接走 SSH，用 OpenSSH agent。服务器得是 Linux。

## 出问题时

**`auth failed: Authentication failed.`**
密码错了，或者你的密钥不在服务器的 `authorized_keys` 里。用 `gpuwho add NAME HOST --password --force` 重新添加，再按 `r`。

**`python3: not found`（后面跟着 `rust agent: ...`）**
Rust 版在那台服务器上跑不起来（原因写在后面），又没有 `python3` 可以替代。常见原因是 `/run/user` 挂载成了 noexec，装上 `python3` 就好。

**`host key changed for ...`**
服务器的主机密钥和第一次连接时不一样了。如果知道原因（比如重装系统），运行 `ssh-keygen -R HOST`，再删掉 `~/.config/gpuwho/known_hosts` 里那一行。

**用户那一栏显示 `? (hidden)` 或 `? (gone)`**
`hidden`：那台服务器的 `/proc` 用了 `hidepid` 挂载，看不到别人的进程。`gone`：进程已经退出但驱动还占着显存，一般是被杀掉的任务留下的 DataLoader worker。

**某人的内存和 `ps`、`htop` 加起来的不一样**
gpuwho 显示的是内核记在这个用户 cgroup 上的内存，包括 `/dev/shm` 里的文件，不包括页缓存。进程表里的内存还是每个进程的 RSS。

## 开发

```sh
git clone https://github.com/htyvv/gpuwho && cd gpuwho
cargo test --workspace            # 客户端和服务器端
python -m pytest                  # Python 版服务器端
cargo run -p gpuwho -- demo
```

想在本地编译时带上 Rust 版服务器端，先编译它再告诉客户端路径：`cargo build --release -p gpuwho-agent && GPUWHO_AGENT_X86_64=target/release/gpuwho-agent cargo build --release -p gpuwho`。不带的话，所有服务器都用 `python3` 版。`python scripts/screenshot.py target/release/gpuwho docs/screenshot.png` 用演示数据重画 README 里的图（需要 Pillow）。

## 致谢

NVML 的处理方式（进程 API 的版本回退、memory v2、掉卡、按进程的利用率）参考了 [gpustat](https://github.com/wookayin/gpustat) 和 [nvitop](https://github.com/XuehaiPan/nvitop)。终端界面用的是 [ratatui](https://github.com/ratatui/ratatui)。

## 许可证

[MIT](LICENSE)
