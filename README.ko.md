# gpuwho

[![CI](https://github.com/htyvv/gpuwho/actions/workflows/ci.yml/badge.svg)](https://github.com/htyvv/gpuwho/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/htyvv/gpuwho)](https://github.com/htyvv/gpuwho/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-green)](LICENSE)

**연구실 GPU 서버 전체를 위한 nvidia-smi. SSH만 있으면 됩니다.**

어느 서버의 몇 번 GPU를 누가, 얼마나 쓰고 있는지 터미널 하나에서 봅니다.
서버에는 아무것도 설치하지 않습니다. 600 KB짜리 에이전트를 SSH로 복사해 두거나(또는 `python3`로 파이썬 에이전트를 돌려서) NVML을 직접 읽습니다.

[English](README.md) | [简体中文](README.zh-CN.md) | 한국어

<p align="center">
  <img src="docs/demo.gif" alt="서버 2대, GPU 16장에서 작업이 끝나고 새로 시작되는 모습을 보여주는 gpuwho" width="100%">
  <br>
  <sub><code>gpuwho demo</code>의 가짜 서버와 사용자입니다. node01에서 작업 하나가 끝나고 node03에서 새 작업이 시작됩니다.</sub>
</p>

```sh
curl -fsSL https://raw.githubusercontent.com/htyvv/gpuwho/main/install.sh | sh
gpuwho demo                   # 서버 없이 먼저 써 보기
gpuwho add gpu1 10.0.0.11     # 그다음 내 서버 등록
gpuwho
```

## 보이는 것

- 모든 서버의 모든 GPU: 사용률, 메모리, 온도, 팬, 전력, 그리고 쓰는 중일 때는 인코더/디코더까지.
- GPU마다 누가 메모리를 얼마나 잡고 있는지. Docker로 돌린 작업은 `root@3f9c2a`처럼 컨테이너 ID로 나옵니다.
- 지금 비어 있는 GPU. 프로세스가 없고, 사용 메모리 1 GiB 미만, 사용률 5% 미만일 때만 idle로 칩니다. GPU에 붙어 있는 Xorg나 gnome-shell은 사용자로 세지 않습니다.
- 잡아 두고 안 쓰는 GPU: 1 GiB 넘게 잡은 채 30분 동안 거의 0%인 작업에는 `idle 2h` 표시가 붙고, 사용자별 표에 *Held idle* 열이 생깁니다.
- GPU 하나를 두 작업이 나눠 쓸 때, 어느 쪽이 실제로 연산 중(SM%)이고 어느 쪽이 메모리만 잡고 있는지.
- 서버 전체를 합친 사용자별 표: GPU 몇 장, GPU 메모리, 어디서(`node01[0,1]`), CPU 코어, RAM. RAM은 커널이 그 사용자의 cgroup에 청구한 양이라, fork된 DataLoader 워커가 30번 중복으로 더해지지 않습니다.
- 어떤 프로세스에도 속하지 않는 GPU 메모리, 채굴 프로그램처럼 보이는 프로세스, 연구실 GPU 한도를 넘은 사람(`--max-gpus 2`), 버스에서 떨어진 GPU, MIG 인스턴스.

터미널이 넓으면 서버를 나란히 놓습니다. 다 들어가지 않으면 알아서 서버당 한 줄로 바꾸고(`c`로 전환), 그래도 넘치면 스크롤합니다.

<p align="center">
  <img src="docs/screenshot.png" alt="CPU 전용 서버와 사라진 GPU를 포함해 서버 4대를 나란히 보여주는 gpuwho" width="100%">
</p>

## 연구실에서 같이 쓰기

"누가 쓰고 있나" 다음의 일들: 기다리고, 맡아 두고, 정리하기.

```sh
gpuwho free                                   # 비어 있는 GPU 전부, 남은 메모리가 많은 순
gpuwho free gpu1 -n 2 --wait -- python train.py
                                              # gpu1에서: 2장이 빌 때까지 기다렸다가 CUDA_VISIBLE_DEVICES를 맞춰 실행
gpuwho free -n 4 --wait --notify https://hooks.slack.com/services/...
                                              # Discord, Feishu, WeCom, DingTalk, ntfy, Telegram도 됩니다
gpuwho claim gpu1:2,3 --for 6h --note "ICLR rebuttal"
gpuwho claims                                 # 예약은 모두에게 보이고, `free`는 남이 예약한 GPU를 건너뜁니다
gpuwho unclaim gpu1
gpuwho kill gpu1:3                            # 그 GPU의 내 프로세스만, 목록을 보여주고 물어본 뒤에
```

예약은 잠금이 아니라 메모입니다. 예약 시트처럼 강제하는 건 없습니다. 각 서버의 `/var/tmp/gpuwho-claims`에 파일로 남고, 파일 소유자가 곧 예약한 사람이라 남의 이름으로 예약할 수 없습니다.

```sh
gpuwho record usage.jsonl            # 1분에 한 번 기록. tmux에서 돌리거나, cron에서 `--once`로
gpuwho report usage.jsonl --since 7d # 사람별 GPU-시간과, 잡아 두고 안 쓴 시간
gpuwho once --html > ~/public_html/gpus.html   # cron으로 연구실 공용 페이지 만들기
```

코딩 에이전트용으로 MCP도 지원합니다. Claude Code나 Cursor 같은 도구가 작업을 띄우기 전에 빈 GPU를 확인할 수 있습니다.

```sh
claude mcp add gpuwho -- gpuwho mcp   # 도구: gpu_status, free_gpus, who_uses
```

## 설치

```sh
curl -fsSL https://raw.githubusercontent.com/htyvv/gpuwho/main/install.sh | sh
```

`~/.local/bin`에 실행 파일 하나를 넣습니다(Linux x86_64/aarch64, macOS). Windows는 [Releases](https://github.com/htyvv/gpuwho/releases)에서 `gpuwho-x86_64-pc-windows-msvc.zip`을 받으세요. 직접 빌드하려면 `cargo install --git https://github.com/htyvv/gpuwho gpuwho`.

서버 쪽은 Linux와 NVIDIA 드라이버만 있으면 됩니다. root도, 데몬도, 열어 둘 포트도 필요 없습니다. 연구실 서버들이 NFS로 홈을 공유한다면 한 번만 설치하면 어느 서버에서든 `gpuwho`를 쓸 수 있습니다.

## 사용법

서버마다 한 번씩 등록합니다. 등록할 때 바로 접속해 보고, 되는 것을 확인한 뒤에 저장합니다.

```sh
gpuwho add gpu1 10.0.0.11                  # SSH 키나 ssh-agent 사용
gpuwho add gpu2 alice@10.0.0.12 --password # 비밀번호를 물어보고 저장 (파일 권한 0600)
gpuwho add dgx dgx.example.org:2222 --key ~/.ssh/id_lab
gpuwho add lab-a100                        # ~/.ssh/config의 Host 이름, ProxyJump도 그대로
gpuwho add here local                      # 지금 이 컴퓨터, SSH 없이
```

직접 적지 않은 것(사용자, 포트, 키, 점프 호스트)은 `ssh`가 하듯이 `~/.ssh/config`에서 가져옵니다.

그다음엔 `gpuwho`만 치면 됩니다.

| 키 | |
|---|---|
| `q` | 종료 |
| `p` | 프로세스 표 (PID, 사용자, GPU 메모리, SM%, CPU%, RAM, 실행 시간, 명령) |
| `c` | 서버당 한 줄로 보기 |
| `j` `k` `↑` `↓` `PgUp` `PgDn` `g` `G` | 스크롤 |
| `r` | 실패한 서버 바로 다시 시도 |
| `space` | 일시정지 |
| `?` | 도움말 |

```
gpuwho                       실시간 대시보드, 3초마다 갱신
gpuwho -i 1 -p               1초마다, 프로세스 표와 함께
gpuwho once                  한 번 출력하고 종료 (watch, cron, 파이프로 실행해도 이렇게 동작)
gpuwho once --json           같은 내용을 JSON으로 (형식 고정)
gpuwho test                  모든 서버 접속 확인, 하나라도 실패하면 exit 1
gpuwho list | remove gpu2
```

## 동작 방식

- 서버마다 SSH 연결 하나([russh](https://github.com/Eugeny/russh))와 오래 사는 에이전트 하나. 갱신할 때마다 줄바꿈 하나를 보내고 JSON 한 줄을 받습니다. 매번 핸드셰이크를 하거나 프로세스를 띄우지 않습니다.
- 에이전트는 glibc 2.17 기준으로 빌드한 600 KB짜리 Rust 바이너리입니다(CentOS 7 이상, x86_64·aarch64). 사용자별 런타임 디렉터리(`/run/user/UID`, 로그아웃하면 사라지는 tmpfs)나 `~/.cache/gpuwho`에 한 번 복사해 두고 계속 씁니다. 실행할 수 없는 서버(noexec, 다른 아키텍처)에서는 [`probe.py`](crates/gpuwho/assets/probe.py)를 `python3 -c`로 흘려보냅니다. 같은 JSON이고, 디스크에 아무것도 남기지 않고, Python 3.6부터 됩니다.
- GPU 정보는 `nvidia-smi`가 쓰는 라이브러리인 NVML을 `dlopen`으로 직접 읽습니다. `nvidia-smi`를 실행하거나 그 출력을 파싱하지 않습니다.
- 프로세스 주인은 `/proc`에서, 컨테이너는 `/proc/PID/cgroup`에서 알아냅니다 (Docker, containerd, Podman, CRI-O).
- 내 계정 권한으로 돌고, SSH 세션이 끊기거나, 90초 동안 요청이 없거나, 6시간이 지나면 스스로 종료합니다.
- NVIDIA 드라이버가 멈추면 NVML 호출은 15초 뒤에 포기하고, 그 서버의 CPU와 RAM은 계속 보여줍니다.
- 꺼진 서버는 2, 4, 8 ... 최대 60초 간격으로 다시 시도합니다. 비밀번호가 틀렸거나 호스트 키가 바뀐 서버는 `r`을 누를 때까지 기다리니, 서버의 fail2ban에 걸릴 일이 없습니다.

비용은 우리 서버에서 잰 값입니다.

| | |
|---|---|
| 에이전트, 갱신 한 번 (프로세스 1,750개, GPU 8장) | CPU 31 ms (3초 간격이면 코어의 1%) |
| 클라이언트, 서버 6대 실시간 화면 | 코어의 0.3%, RAM 10 MB, 스레드 3개 |
| 서버 64대 화면 한 장 그리기 | 21 ms |
| 서버 6대 `gpuwho test` | 2.3초, CPU 14 ms |

gpuwho 0.x는 파이썬이었습니다. Rust로 다시 쓰면서 에이전트는 갱신당 47 → 31 ms, 클라이언트는 코어의 5% → 0.3%, 서버 64대 화면은 412 → 21 ms가 됐습니다.

## 왜 만들었나

우리 연구실에는 GPU 서버가 여러 대 있고 홈 디렉터리는 하나를 공유합니다. 빈 GPU를 찾으려면 서버마다 `ssh` 들어가서 `nvidia-smi`를 치고, PID 812345가 누구 건지 `ps`로 다시 찾고, 그다음엔 단톡방에 누가 다 썼는지 물어봐야 했습니다. gpuwho는 앞부분을 모든 서버에 대해 3초마다 대신 해주고, 단톡방 부분에는 약간의 틀을 줍니다.

## 다른 도구와 비교

좋은 GPU 모니터는 이미 많습니다. 2026년 10월 기준으로 각 프로젝트의 README와 소스를 보고 정리한 차이입니다.

| | 여러 서버를 한 화면에 | 서버마다 필요한 것 | GPU 프로세스 주인 표시 | 화면 |
|---|---|---|---|---|
| **gpuwho** | 예 | 없음 (에이전트를 SSH로 복사, 또는 `python3`) | 예, Docker 작업 포함 | 터미널 |
| [nvitop](https://github.com/XuehaiPan/nvitop) | 아니요, 한 대씩 (`ssh -t host nvitop`) | nvitop | 예 | 터미널 |
| [nvtop](https://github.com/Syllo/nvtop) | 아니요, 한 대씩 | nvtop | 예 | 터미널 |
| [gpustat](https://github.com/wookayin/gpustat) | 아니요, 한 대씩 | gpustat | 예 | 터미널 |
| [svtop](https://pypi.org/project/svtop/) | 예 | 없음 (`nvidia-smi` 실행) | 예 | 터미널 |
| [gpustat-web](https://github.com/wookayin/gpustat-web) | 예 | gpustat | 예 | 브라우저 |
| [all-smi](https://github.com/lablup/all-smi) | 예 | SSH 모드는 없음, 전체 정보는 `all-smi api` | SSH 모드에서는 안 됨 | 터미널 |
| [cluster-smi](https://github.com/PatWie/cluster-smi) | 예 | 노드 데몬 | 예 | 터미널 |
| [gpuview](https://github.com/fgaim/gpuview) | 예 | gpuview 서비스 | 일부 | 브라우저 |
| [DCGM exporter](https://github.com/NVIDIA/dcgm-exporter) + Grafana | 예 | exporter, Prometheus | Kubernetes 파드만 | 브라우저 |

이럴 땐 다른 도구가 낫습니다.

- 서버 한 대에서 프로세스 트리, 지난 그래프, 파이썬 API가 필요하다면 **nvitop**.
- 모니터 옆에 파일 전송 창이 필요하다면 **svtop**.
- AMD, Intel, Apple 등 다른 가속기가 있다면 **nvtop**이나 **all-smi**. gpuwho는 NVIDIA 전용입니다.
- 노드가 아주 많고 NVLink 토폴로지나 기록·재생이 필요하다면 **all-smi**.
- 알림과 긴 기록, Kubernetes가 필요하다면 **DCGM exporter**와 Prometheus, Grafana 또는 Netdata.
- 터미널 대신 웹 페이지나 데스크톱 상태바로 보고 싶다면 **gpustat-web**, **gpuview**, [Multiple-smi](https://github.com/ClementPinard/Multiple-smi) (또는 cron으로 `gpuwho once --html`).

gpuwho 덕분에 ssh를 몇 번 덜 하셨다면, star 하나가 다른 연구실이 이 도구를 찾는 데 도움이 됩니다.

## 자주 묻는 것

**AMD나 Intel GPU도 되나요?**
아니요, NVIDIA만 됩니다 (NVML을 씁니다). NVIDIA GPU가 없는 서버도 CPU, RAM, 사용자는 보여줍니다.

**`~/.ssh/config`를 읽나요? 점프 호스트는요?**
네. `ssh -G`로 최종 설정을 물어보기 때문에 `Include`, `Match`, `ProxyJump`, `ProxyCommand`가 모두 됩니다. 점프 호스트는 내 `ssh`가 키나 agent로 접속하고, 저장된 비밀번호는 최종 서버에만 보냅니다.

**비밀번호를 저장해도 안전한가요?**
`~/.config/gpuwho/servers.json`에 평문으로 저장되고, gpuwho가 시작할 때마다 이 파일 권한을 0600으로 맞춥니다. 가능하면 SSH 키를 쓰세요. [SECURITY.md](SECURITY.md)에 자세히 있습니다.

**서버에 무엇이 남나요?**
`/run/user/UID`(로그아웃하면 사라짐)나 `~/.cache/gpuwho`의 에이전트 파일, 그리고 `claim`을 했다면 `/var/tmp/gpuwho-claims`의 내 예약 파일입니다. `servers.json`에서 그 서버에 `"agent": "python"`을 넣으면(또는 `gpuwho add --agent python`) 아무것도 쓰지 않습니다.

**Windows에서 되나요?**
네, Windows 빌드가 직접 SSH로 접속하고 OpenSSH agent를 씁니다. 서버는 Linux여야 합니다.

## 문제 해결

**`auth failed: Authentication failed.`**
비밀번호가 틀렸거나, 키가 서버의 `authorized_keys`에 없습니다. `gpuwho add NAME HOST --password --force`로 다시 등록하고 `r`을 누르세요.

**`python3: not found` (뒤에 `rust agent: ...`가 붙음)**
Rust 에이전트를 그 서버에서 실행할 수 없었고(이유가 뒤에 나옵니다), 대신 쓸 `python3`도 없는 경우입니다. 보통 `/run/user`가 noexec로 마운트된 탓이고, `python3`를 설치하면 해결됩니다.

**`host key changed for ...`**
처음 본 뒤로 서버의 호스트 키가 바뀌었습니다. 재설치처럼 이유를 안다면 `ssh-keygen -R HOST`를 실행하고 `~/.config/gpuwho/known_hosts`에서 그 줄을 지우세요.

**사용자 칸에 `? (hidden)`이나 `? (gone)`이 나와요**
`hidden`: 그 서버의 `/proc`가 `hidepid`로 마운트되어 다른 사람의 프로세스가 보이지 않습니다. `gone`: 프로세스는 끝났는데 드라이버가 아직 메모리를 잡고 있는 경우로, 대개 죽은 작업의 DataLoader 워커입니다.

**사용자 RAM이 `ps`나 `htop`에서 더한 값과 달라요**
gpuwho는 커널이 그 사용자의 cgroup에 청구한 메모리를 보여줍니다. `/dev/shm`의 파일은 포함되고 페이지 캐시는 빠집니다. 프로세스 표의 RAM은 프로세스별 RSS입니다.

## 개발

```sh
git clone https://github.com/htyvv/gpuwho && cd gpuwho
cargo test --workspace            # 클라이언트와 에이전트
python -m pytest                  # 파이썬 대체 에이전트
cargo run -p gpuwho -- demo
```

로컬 빌드에 Rust 에이전트를 넣으려면 먼저 빌드해서 경로를 알려주세요: `cargo build --release -p gpuwho-agent && GPUWHO_AGENT_X86_64=target/release/gpuwho-agent cargo build --release -p gpuwho`. 없으면 모든 서버에서 `python3` 에이전트를 씁니다. `python scripts/screenshot.py target/release/gpuwho docs/screenshot.png`는 데모 데이터로 README 이미지를 다시 그립니다 (Pillow 필요).

## 감사의 말

NVML 처리 방식(프로세스 API 폴백, 메모리 v2, 사라진 GPU, 프로세스별 사용률)은 [gpustat](https://github.com/wookayin/gpustat)과 [nvitop](https://github.com/XuehaiPan/nvitop)을 참고했습니다. 터미널 UI는 [ratatui](https://github.com/ratatui/ratatui)로 만들었습니다.

## 라이선스

[MIT](LICENSE)
