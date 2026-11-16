# wits

**witr 告诉你进程为什么在运行，wits 告诉你它为什么慢。**

解释 Linux 上一个进程为什么慢：原因是什么、占了它多少时间、具体卡在哪个文件、哪把锁或哪个
socket 上，以及该怪哪个进程。不需要 root，不需要 eBPF，只有一个静态链接的二进制文件。

[English](README.md)

```console
$ wits deploy
deploy (pid 2655) is waiting for a lock: 100% of its time over the last 2.0s
  what   the file lock on /root/wits-demo/schema.lock
  blame  held by pid 2652 (migrate …/schema.lock …/warehouse 600), which is only waiting for its child pid 2654 (warehouse)

$ wits backup
backup (pid 2570) is waiting for disk: 74% of its time over the last 2.0s
  what   write() on /root/wits-demo/backup.img, device vda (70% busy)
  blame  its own I/O: it read 0 B, wrote 30.5 MB; no other visible process is doing more
  also   cpu 26%

$ wits report
report (pid 2577) is waiting for another process: 100% of its time over the last 2.0s
  what   blocked in read() on fd 0, FIFO /root/wits-demo/rows
  blame  pid 2576 (warehouse 600), which is asleep on its own timer
```

![wits 演示](demo/demo.gif)

`top` 只给你一个数字。`wits` 直接给出结论，也就是你用 `top`、`iotop`、`strace`、`lsof` 和
`/proc/locks` 查上十分钟才能写出来的那句话。输出为英文。

## 安装

```sh
curl -fsSL https://raw.githubusercontent.com/aryehlev/wits/main/install.sh | sh
```

或者任选一种：

```sh
cargo install wits
cargo binstall wits
brew install aryehlev/tap/wits     # Linux 上的 Homebrew
yay -S wits-bin                    # Arch (AUR)
```

也可以从 [Releases](https://github.com/aryehlev/wits/releases) 下载 x86_64 或 aarch64
的二进制文件，放进 `PATH` 即可。它是静态链接的，没有任何依赖。

## 用法

```sh
wits <pid|进程名>   # 这个进程为什么慢？
wits               # 现在哪些进程等待得最久？
```

| 选项 | 含义 |
| --- | --- |
| `-t, --time <秒>` | 观察多久（默认 2 秒） |
| `--json` | 机器可读的输出，供脚本和编程智能体使用 |
| `--no-follow` | 不要跟进到进程正在等待的子进程 |

如果你问的是一个只是在等子进程的 shell 或 `make`，wits 会顺着链条往下找，解释真正在等待的
那个子进程。如果进程根本不慢（在定时器上睡眠、在等终端输入、服务器没有请求），wits 会直接
说明，而不会硬编一个原因。

## 六种原因

| 原因 | 进程正在… | wits 指出 | 归咎于 |
| --- | --- | --- | --- |
| `cpu` | 忙于计算，没有任何东西挡着它 | 用户态/内核态占比，最热的线程 | 它自己 |
| `cpu-wait` | 可以运行，但拿不到 CPU | 它可用的 CPU，或 cgroup 的 CPU 限额 | 占着这些 CPU 的进程，或限额本身 |
| `disk` | 阻塞在存储 I/O 上 | 系统调用、文件、设备及其繁忙程度 | I/O 最多的进程 |
| `memory` | 在换页，或反复淘汰并重新读入自己的页 | 交换活动、主缺页次数 | cgroup 内存限额，或占内存最多的进程 |
| `lock` | 在排队等文件锁或互斥锁 | 被锁的文件，或互斥锁地址 | 持有锁的进程或线程，以及它在做什么 |
| `peer` | 在等另一个进程或网络 | 管道、socket 和地址 | 另一端的进程，或远程主机 |

只有这六种，wits 不会猜测这之外的原因。

## 在编程智能体中使用

`skills/why-is-this-slow/SKILL.md` 教会编程智能体（Claude Code 以及其他支持 skill 的工具）
什么时候运行 wits、如何解读结果。把这个目录复制到你的 skills 目录：

```sh
mkdir -p ~/.claude/skills
cp -r skills/why-is-this-slow ~/.claude/skills/
```

然后直接问：“测试跑得特别慢，为什么？”智能体会运行 `wits --json`，读取 `cause`、`what`
和 `blame`，根据事实而不是猜测来行动。

## 工作原理

wits 观察进程的每个线程两秒钟，只读取内核本来就公开的文件：

- `/proc/<pid>/task/*/schedstat` 给出线程在 CPU 上运行的精确纳秒数，以及可运行但在排队等
  CPU 的纳秒数。这两项是实测值，不是采样值。
- 线程其余的时间靠采样来划分：每 10 毫秒读一次线程状态（`stat`）、它阻塞在哪个系统调用及
  其参数（`syscall`）、以及它睡在哪个内核函数里（`wchan`）。
- 系统调用里的文件描述符通过 `/proc/<pid>/fd` 解析成文件、管道或 socket。`/proc/locks`
  给出文件锁的持有者。`/proc/<pid>/net/tcp` 和无需特权的 `sock_diag` 接口给出 socket
  的地址、最近一次收发的时间以及另一端的 socket；再扫描 `/proc/*/fd` 找到拥有它的进程。
- 对于有竞争的互斥锁，持有者的线程号直接从锁本身读出。
- cgroup 文件（`cpu.stat`、`cpu.max`、`memory.events`）反映 CPU 节流和内存限额；
  `/proc/diskstats` 和 `/proc/*/io` 反映设备负载以及是谁造成的。

没有内核模块，没有 tracing，需要的权限不超过 `ps` 和 `cat`。代价见下一节。

## 什么时候会出错

- **它只看两秒。** 时有时无的卡顿可能被错过，请用 `-t 30`。
- **空闲和等待有时看起来一样。** 一个阻塞在 socket 读上的线程，可能是在等一个慢服务器，也
  可能只是没事可做。只有一个连接的进程，wits 称为“等待”；有很多连接的事件循环称为“空闲”，
  除非其中某个连接发出了请求却还没有收到回复。
- **互斥锁是按惯例识别的。** glibc、Rust 标准库和 Go 运行时让等锁的线程以同样的方式休眠，
  wits 依赖这一点。只有 glibc 的互斥锁能指出持有者。其他锁（Python GIL、Go 的
  `sync.Mutex`、`parking_lot`）会显示为休眠的线程，而不是 `lock`。
- **其他用户的进程大多看不透。** 不用 `sudo` 时，wits 能看到它们的 CPU 时间、等 CPU 的时间
  和等磁盘的时间，但看不到具体是哪个文件、哪把锁或哪个 socket。遇到这种情况它会明确告诉你。
- **Ubuntu 默认的 `ptrace_scope=1` 会隐藏细节：** 对不是你 shell 后代的进程，wits 只能退回
  到内核等待函数，并把答案标记为低可信度。`sudo wits` 可以看到全部。
- **不用 `sudo` 时磁盘归因不完整：** 其他用户进程的 I/O 不可见，wits 可能会说“没有可见的
  元凶”。
- **内存需要证据。** 如果没有换入，也没有触到 cgroup 限额，从磁盘读回的页会被报告为 `disk`。
- **虚拟机：** 被 hypervisor 拿走的 CPU 时间会被报告为 `cpu-wait`，且找不到责任方。
- **容器：** 位于其他命名空间的进程和 socket 无法指名。
- **完全不涉及：** GPU 等待，以及进程内部从不进入内核的问题（慢算法只会显示为 `cpu`）。

[`cases/`](cases) 里的每个脚本制造一种慢的情况，并写明 wits 必须给出的答案。
`cases/run.sh` 会全部运行一遍，这就是测试套件。

## 系统要求

x86_64 或 aarch64 上的 Linux。内核 5.16 及以上效果最好；在更旧的内核上，如果读不到系统
调用信息，细节会少一些。

## wits 已经完成

它只做一件事。没有路线图：不会增加新的原因，没有 TUI，没有守护进程，也不会移植到 macOS。
欢迎报告 bug，bug 会被修复。

## 许可证

MIT
