# wits

**witr 告诉你进程为什么在运行，wits 告诉你它为什么慢。**

解释一个进程为什么慢：原因是什么、占了它多少时间、具体卡在哪个文件、哪把锁或哪个 socket 上、
该怪哪个进程，以及时间花在了代码的哪里。支持 Linux 和 macOS，支持任何语言，只有一个二进制
文件。不需要 root；有 root 能看到更多。

[English](README.md)

```console
$ wits deploy
deploy (pid 2655) is waiting for a lock: 100% of its time over the last 2.0s
  what   the file lock on /root/wits-demo/schema.lock
  blame  held by pid 2652 (migrate …/schema.lock …/warehouse 600), which is only waiting for its child pid 2654 (warehouse)

$ wits fs
fs (pid 4148) is waiting for disk: 97% of its time over the last 2.0s
  what   fsync() on /home/ubuntu/t/log.bin, device sda1 (100% busy)
  where  100% in flush_log (…/ubuntu/t/fs.c:4) ← main (…/ubuntu/t/fs.c:5)
  blame  its own I/O: it read 0 B, wrote 5.3 MB; no other visible process is doing more
  also   cpu 3%

$ wits shop/api-7d9f
python3 (pid 6723, pod shop/api-7d9f/api) is waiting for another process: 100% of its time over the last 2.0s
  what   blocked in recvfrom() on fd 3, TCP 10.20.0.1:45476 → 10.20.0.2:5432 (postgres)
  blame  pid 6719 (python3 …/server.py, pod db/pg-0/postgres), which is itself waiting for disk
```

![wits 演示](demo/demo.gif)

`top` 只给你一个数字。`wits` 直接给出结论，也就是你用 `top`、`iotop`、`strace`、`lsof`、
`/proc/locks` 和性能分析器查上十分钟才能写出来的那句话。输出为英文。

## 安装

```sh
curl -fsSL https://raw.githubusercontent.com/aryehlev/wits/main/install.sh | sh
```

或者任选一种：

```sh
cargo install wits
cargo binstall wits
brew install aryehlev/tap/wits     # macOS 和 Linux
yay -S wits-bin                    # Arch (AUR)
```

也可以从 [Releases](https://github.com/aryehlev/wits/releases) 下载二进制文件，放进
`PATH` 即可：Linux 版（x86_64 和 aarch64）是静态链接的，macOS 版支持 Apple 芯片和 Intel。
Kubernetes 的用法见[下文](#kubernetes)。

## 用法

```sh
wits <pid|进程名>             # 这个进程为什么慢？慢在代码的哪里？
wits                         # 现在哪些进程等待得最久？
wits profile <pid|进程名>     # 每一种原因，以及它最热的调用路径
wits run -- <命令...>         # 运行一条命令：它的时间花到哪里去了？
wits diff before.json after.json   # 我的改动有没有用？
wits <命名空间>/<pod>          # 一个 pod 里最忙的进程（Linux）
```

| 选项 | 含义 |
| --- | --- |
| `-t, --time <秒>` | 观察多久（默认 2 秒） |
| `--json` | 机器可读的输出，供脚本和编程智能体使用 |
| `--no-follow` | 不要跟进到进程正在等待的子进程 |
| `--no-stacks` | 不收集调用栈（没有 `where`） |
| `--folded` | 与 `profile` 一起用：输出折叠栈，供 `flamegraph.pl` 使用 |
| `-o <文件>` | 与 `run` 一起用：保存报告，供 `diff` 使用 |
| `-n <次数>` | 与 `run` 一起用：把命令运行这么多次 |
| `--peers <列表>` | 其他机器上的 wits agent，用来把归因跟进过去 |

如果你问的是一个只是在等子进程的 shell 或 `make`，wits 会顺着链条往下找，解释真正在等待的
那个子进程。如果进程根本不慢（在定时器上睡眠、在等终端输入、服务器没有请求），wits 会直接
说明，而不会硬编一个原因。

## 六种原因

| 原因 | 进程正在… | wits 指出 | 归咎于 |
| --- | --- | --- | --- |
| `cpu` | 忙于计算，没有任何东西挡着它 | 用户态/内核态占比，最热的线程，最热的函数 | 它自己 |
| `cpu-wait` | 可以运行，但拿不到 CPU | 它可用的 CPU，或 cgroup 的 CPU 限额 | 占着这些 CPU 的进程，或限额本身 |
| `disk` | 阻塞在存储 I/O 上 | 系统调用、文件、设备及其繁忙程度 | I/O 最多的进程 |
| `memory` | 在换页，或反复淘汰并重新读入自己的页 | 交换活动、主缺页次数 | cgroup 内存限额，或占内存最多的进程 |
| `lock` | 在排队等文件锁或互斥锁 | 被锁的文件，或互斥锁地址 | 持有锁的进程或线程，以及它在做什么 |
| `peer` | 在等另一个进程或网络 | 管道、socket 和地址 | 另一端的进程（在任何容器或 pod 中，或者在装了 agent 的其他节点上），或远程主机 |

只有这六种，wits 不会猜测这之外的原因。

## 慢在代码的哪里

`where` 这一行指出造成该原因的函数、文件和行号，以及它的调用者。`wits profile` 列出每一种
原因及其最热的调用路径：

```console
$ sudo wits profile lockm
lockm (pid 9733) is waiting for a lock: 99% of its time over the last 2.0s
  stacks from ebpf+stack-scan

  lock 99% (2.0s)
    100%  main (…/wits/cases/lock-mutex.c:21)

  cpu 1% (12ms)
    100%  holder (…/wits/cases/lock-mutex.c:13) in clock_nanosleep

  idle, not counted (2.0s)
    100%  holder (…/wits/cases/lock-mutex.c:13)
```

调用栈按原因划分，而不只是按 CPU 时间：一个把时间花在 `fsync` 上的函数会出现在 `disk` 下，
一个在互斥锁上排队的函数会出现在 `lock` 下。这正是只看 CPU 的性能分析器看不到的部分。

调用栈是这样收集的：

| | 在 CPU 上的时间 | 阻塞的时间 |
| --- | --- | --- |
| Linux，root | eBPF：499 Hz 的 CPU 时钟采样 | eBPF：每一次上下文切换，连同它的调用栈和离开 CPU 的时长 |
| Linux，非 root | 没有（会有一条说明） | 每个阻塞线程的调用栈，从 `/proc/<pid>/mem` 读取 |
| macOS | `/usr/bin/sample` | `/usr/bin/sample` |

eBPF 程序在 wits 内部汇编，通过 `bpf()` 系统调用加载：不需要编译器，不需要 BTF，不需要
libbpf，什么都不用装。

语言：

- **编译型**（C、C++、Rust、Go、Zig、Swift…）：函数、文件和行号来自二进制文件的符号和
  DWARF，包括内联函数。用帧指针编译（`-fno-omit-frame-pointer`，Rust 用
  `-C force-frame-pointers=yes`）才能得到完整的调用栈；Go 默认就有。调用栈被截断时
  wits 会说明。
- **JIT 和解释型**（Node、Python 3.12+、Java、.NET、Ruby、Erlang）：运行时发布 perf map
  时，它们的函数就会显示出来。wits 会告诉你每种运行时的开关（`node --perf-basic-prof`、
  `PYTHONPERFSUPPORT=1`、`jcmd <pid> Compiler.perfmap`、`DOTNET_PerfMapEnabled=1`、
  `--yjit-perf`、`+JPperf true`），而 `wits run` 会替你设置那些环境变量形式的开关。
- **macOS** 只显示原生栈帧：解释型代码的函数看不到。

## 改进循环

`wits run` 启动一条命令，观察它以及它启动的所有进程直到退出，然后报告时间花到了哪里，
精确到函数。保存报告，修改代码，再运行一次，`wits diff` 会告诉你改动有没有用、差异是否
超出了每次运行之间的波动，以及哪些函数变了：

```console
$ wits run -n 5 -o before.json -- ./work 400
wits: `./work 400` took 515ms (median of 5, 463ms–646ms) (exit 0); 1 process, 435ms on CPU
  cpu        91%   435ms   work 428ms
  cpu-wait    9%    44ms   work 44ms
  where     cpu: 100% in inner_loop (…/wits/cases/where.c:14) ← main (…/wits/cases/where.c:33)
saved to before.json; after a change, run it again with -o new.json and: wits diff before.json new.json

$ wits run -n 5 -o after.json -- ./work 100
$ wits diff before.json after.json
faster: 515ms → 165ms (−68%), beyond run-to-run noise (the runs do not overlap)
  on CPU    435ms → 146ms (−66%)
  cpu       435ms → 146ms (−289ms)
  cpu-wait  44ms → 19ms (−25ms)

  functions that changed most
     −302ms  inner_loop (…/wits/cases/where.c)  cpu 360ms → 58ms
```

如果两组运行时间有重叠，它会说 `no clear change … within run-to-run noise`，而不会宣称
变快了。如果每边只运行一次，它会提示你使用 `-n`。`wits run` 不需要 root：它启动的一切都是
它自己的后代进程。

## Kubernetes

wits 会为它报告的每个进程标出所在的 pod 和容器，信息来自 cgroup 路径和 kubelet 的日志
目录，不需要访问 API。在节点上，`wits <命名空间>/<pod>[/<容器>]` 解释一个 pod 里最忙的
进程；如果一个进程在等某个连接，wits 会跟进到节点上持有另一端的容器或 pod，经过 Service
地址也可以。

要跨节点追查归因，在每个节点上运行 agent：

```sh
kubectl create namespace wits
kubectl -n wits create secret generic wits-token --from-literal=token="$(head -c 32 /dev/urandom | base64)"
kubectl apply -f deploy/kubernetes/wits.yaml
cp deploy/kubectl-wits ~/.local/bin/           # kubectl 插件
kubectl wits shop/api-7d9f
```

`kubectl wits` 在 pod 所在节点的 agent 里运行 wits。当答案是一条通往其他节点的连接时，wits
会询问其他 agent（全部 agent，通过一个 headless Service 找到，没有中心服务器）谁持有另一端。
持有者所在节点的 agent 用同样的方式解释那个进程，答案会显示在 blame 下面：

```console
python3 (pid 2) is waiting for another process: 100% of its time over the last 2.0s
  what   blocked in recvfrom() on fd 3, TCP 10.233.0.1:38940 → 10.233.0.2:5432 (postgres); last sent 576 ms ago, no reply since
  blame  pid 2 (python3) on node node-b
         ↳ python3 (pid 2) is waiting for disk: 92% of its time over the last 2.0s
           what  fsync() on /home/ubuntu/wits/cases/.work/srv.log, device sda1 (68% busy)
           blame its own I/O: it read 0 B, wrote 268 KB; no other visible process is doing more
```

（来自 `cases/agent-follow.sh`，其中两个节点是同一台机器上的两个命名空间，所以 pid 很小。）
它能穿过 Service 的 DNAT，因为它匹配的是客户端自己的地址，而服务端看到的正是这个地址。
最多跟进三跳。

agent 能做什么，供你判断：它以特权模式和 `hostPID` 运行，这样才能读取节点上的每个进程并
加载 eBPF 程序；它以只读方式挂载 `/var/log` 来识别 pod。它不改变任何东西。agent 只回应带有
共享令牌的请求，NetworkPolicy 只允许其他 agent 访问它，而 `kubectl wits` 通过
`kubectl exec` 访问 agent，所以谁能提问由你的 RBAC 决定。在 Kubernetes 之外也可以这样用：
在每台机器上运行 `wits agent --listen <地址:端口>`，在客户端用 `--peers`（或
`WITS_PEERS`），令牌放在 `WITS_TOKEN` 里。

## macOS

命令、答案和 JSON 都一样。不用 `sudo` 时，wits 能完整看到你自己的进程：来自 `libproc` 的
每线程 CPU 时间和排队时间，来自 `/usr/bin/sample` 的带文件和行号的调用栈，以及用来找到另一端
进程的文件描述符、管道和 socket。其他用户的进程只能看到 CPU 时间和运行状态。

与 Linux 的不同：互斥锁的持有者是最佳猜测（macOS 不用 root 就不公开锁的持有者），会注明
"probably"；阻塞的 `read()` 在哪个描述符上是推断出来的，候选有好几个时可信度较低；
`sample` 需要一点时间才能附加上去，所以在 `wits run` 中，存活不到 60 毫秒的进程没有调用栈，
不到半秒的运行统计到的函数时间会偏少；macOS 上没有 agent。

## 在编程智能体中使用

`skills/why-is-this-slow/SKILL.md` 教会编程智能体（Claude Code 以及其他支持 skill 的工具）
什么时候运行 wits、如何解读结果，以及如何用 `wits run` 和 `wits diff` 检验自己的修复。把这个
目录复制到你的 skills 目录：

```sh
mkdir -p ~/.claude/skills
cp -r skills/why-is-this-slow ~/.claude/skills/
```

然后直接问：“测试跑得特别慢，为什么？”智能体会运行 `wits --json`，读取 `cause`、`what`、
`where` 和 `blame`，根据事实而不是猜测来行动。改完代码后，它会运行 `wits diff`，告诉你改动
有没有用。

## 工作原理

在 Linux 上，wits 观察进程的每个线程两秒钟：

- `/proc/<pid>/task/*/schedstat` 给出线程在 CPU 上运行的精确纳秒数，以及可运行但在排队等
  CPU 的纳秒数。这两项是实测值，不是采样值。
- 线程其余的时间靠采样来划分：每 10 毫秒读一次线程状态（`stat`）、它阻塞在哪个系统调用及
  其参数（`syscall`）、以及它睡在哪个内核函数里（`wchan`）。
- 系统调用里的文件描述符通过 `/proc/<pid>/fd` 解析成文件、管道或 socket。`/proc/locks`
  给出文件锁的持有者。`/proc/<pid>/net/tcp` 和无需特权的 `sock_diag` 接口给出 socket
  的地址、最近一次收发的时间以及另一端的 socket；再在每个网络命名空间里扫描 `/proc/*/fd`，
  找到拥有它的进程。
- 对于有竞争的互斥锁，持有者的线程号直接从锁本身读出。
- cgroup 文件（`cpu.stat`、`cpu.max`、`memory.events`）反映 CPU 节流和内存限额；
  `/proc/diskstats` 和 `/proc/*/io` 反映设备负载以及是谁造成的。
- 调用栈来自 eBPF（root 时）或每个阻塞线程在 `/proc/<pid>/mem` 中的栈，用 `blazesym`
  根据二进制文件的符号和 DWARF 或运行时的 perf map 解析成函数名。

## 什么时候会出错

- **它只看两秒。** 时有时无的卡顿可能被错过，请用 `-t 30`。
- **空闲和等待有时看起来一样。** 一个阻塞在 socket 读上的线程，可能是在等一个慢服务器，也
  可能只是没事可做。只有一个连接的进程，wits 称为“等待”；有很多连接的事件循环称为“空闲”，
  除非其中某个连接发出了请求却还没有收到回复。
- **互斥锁是按惯例识别的。** glibc、Rust 标准库和 Go 运行时让等锁的线程以同样的方式休眠，
  wits 依赖这一点。只有 glibc 的互斥锁能指出持有者。有调用栈时，其他运行时的锁函数
  （`parking_lot`、Go 的 `sync.Mutex`、JVM 的 monitor、Python GIL）会被识别为 `lock`。
- **调用栈需要帧指针。** 不带帧指针编译的原生代码调用栈会很短，wits 会指出是哪个库造成的。
  解释型代码显示的是解释器的函数，除非运行时发布了 perf map。
- **Linux 上不用 root 就没有 CPU 上的调用栈**；macOS 上存活不到 60 毫秒的进程也没有。
- **其他用户的进程大多看不透。** 不用 `sudo` 时，wits 能看到它们的 CPU 时间、等 CPU 的时间
  和等磁盘的时间，但看不到具体是哪个文件、哪把锁或哪个 socket。遇到这种情况它会明确告诉你。
- **Ubuntu 默认的 `ptrace_scope=1` 会隐藏细节：** 对不是你 shell 后代的进程，wits 只能退回
  到内核等待函数，并把答案标记为低可信度。`sudo wits` 可以看到全部，`wits run` 不受影响。
- **不用 `sudo` 时磁盘归因不完整：** 其他用户进程的 I/O 不可见，wits 可能会说“没有可见的
  元凶”。
- **内存需要证据。** 如果没有换入，也没有触到 cgroup 限额，从磁盘读回的页会被报告为 `disk`。
- **虚拟机：** 被 hypervisor 拿走的 CPU 时间会被报告为 `cpu-wait`，且找不到责任方。
- **其他节点需要 agent。** 没有 agent 时，远程地址就只是一台远程主机。
- **完全不涉及：** GPU 等待，以及进程内部从不进入内核的问题（慢算法只会显示为 `cpu`，但现在
  会指出慢在哪个函数）。

[`cases/`](cases) 里的每个脚本制造一种慢的情况，并写明 wits 必须给出的答案。
`cases/run.sh` 会全部运行一遍，这就是测试套件。再用 root 运行一次，可以覆盖 eBPF 调用栈、
cgroup 限额、pod 和 agent。

## 系统要求

x86_64 或 aarch64 上的 Linux，内核 5.16 及以上效果最好（在更旧的内核上，如果读不到系统调用
信息，细节会少一些）；eBPF 调用栈需要 4.9 及以上。Apple 芯片或 Intel 上的 macOS。

## 许可证

MIT
