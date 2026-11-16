# wits

**witr tells you why it is running. wits tells you why it is slow.**

Explain why a process is slow on your Linux system: the cause, how much of its
time it costs, the exact file, lock or socket involved, and which other process
is to blame. No root, no eBPF, one static binary.

[简体中文](README.zh-CN.md)

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

$ wits api
api (pid 2580) is waiting for a CPU: 67% of its time over the last 2.0s
  what   runnable but queued: it may only use CPU 0, which is taken
  blame  pid 2579 (encoder) used 0.3 CPUs in the same window; next pid 2578 (encoder) 0.3
  also   cpu 33%
```

![wits demo](demo/demo.gif)

`top` shows you a number. `wits` gives you the sentence you would have written
after ten minutes with `top`, `iotop`, `strace`, `lsof` and `/proc/locks`.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/aryehlev/wits/main/install.sh | sh
```

Or any of:

```sh
cargo install wits
cargo binstall wits
brew install aryehlev/tap/wits     # Homebrew on Linux
yay -S wits-bin                    # Arch (AUR)
```

Or download the binary for x86_64 or aarch64 from
[Releases](https://github.com/aryehlev/wits/releases) and put it on your
`PATH`. It is statically linked and has no dependencies.

## Use

```sh
wits <pid|name>    # why is this process slow?
wits               # which processes are waiting most right now?
```

With no arguments you get something to look at within two seconds:

```console
$ wits
Processes waiting most right now (2.0s sample):
    pid  name         waiting  why
   2655  deploy          100%  waiting for a lock: the file lock on /root/wits-demo/schema.lock ← held by pid 2652 (migrate …)
   2651  backup           77%  waiting for disk: write() on /root/wits-demo/backup.img, device vda (73% busy)
   2661  api              67%  waiting for a CPU: runnable but queued: it may only use CPU 0, which is taken ← pid 2660 (encoder) …
Run `wits <pid>` for the full answer on one of them.
```

| Option | Meaning |
| --- | --- |
| `-t, --time <secs>` | how long to watch (default 2) |
| `--json` | machine-readable output, for scripts and coding agents |
| `--no-follow` | do not descend into the child a process is waiting for |

If you ask about a shell or a `make` that is only waiting for a child, wits
follows the chain down and explains the child that is actually doing the
waiting. If the process is not slow at all (asleep on a timer, waiting at a
prompt, a server with no requests) wits says so instead of inventing a cause.

## The six causes

| Cause | The process is… | wits names | and blames |
| --- | --- | --- | --- |
| `cpu` | busy computing; nothing is in its way | user/kernel split, hottest thread | itself |
| `cpu-wait` | ready to run but not given a CPU | the CPUs it may use, or its cgroup's CPU limit | the process hogging those CPUs, or the limit |
| `disk` | blocked on storage | the system call, the file, the device and how busy it is | the process doing the most I/O |
| `memory` | swapping, or evicting and re-reading its own pages | swap activity, major faults | the cgroup memory limit, or the biggest process |
| `lock` | queued on a file lock or a mutex | the locked file, or the mutex address | the process or thread holding it, and what that holder is doing |
| `peer` | waiting for another process or the network | the pipe, socket and address | the process at the other end, or the remote host |

That is the whole list. wits does not guess beyond it.

## Use it from a coding agent

`skills/why-is-this-slow/SKILL.md` teaches a coding agent (Claude Code and
others that read skills) when to run wits and how to read the answer. Copy the
folder into your skills directory:

```sh
mkdir -p ~/.claude/skills
cp -r skills/why-is-this-slow ~/.claude/skills/
```

Then ask: *"the test run is crawling, why?"* The agent runs `wits --json`,
reads `cause`, `what` and `blame`, and acts on a fact instead of a guess.

## How it works

wits watches every thread of the process for two seconds and reads only files
the kernel already publishes:

- `/proc/<pid>/task/*/schedstat` gives exact nanoseconds on CPU and
  nanoseconds spent runnable but waiting for one. These two are measured, not
  sampled.
- The rest of each thread's time is split by sampling, every 10 ms, its state
  (`stat`), the system call it is blocked in and its arguments (`syscall`), and
  the kernel function it sleeps in (`wchan`).
- The file descriptor in that system call is resolved through `/proc/<pid>/fd`
  to a file, pipe or socket. `/proc/locks` names the holder of a file lock.
  `/proc/<pid>/net/tcp` and the unprivileged `sock_diag` interface give the
  address of a socket, when it last sent and received, and the socket at the
  other end; scanning `/proc/*/fd` finds the process that owns it.
- For a contended mutex, the owner's thread id is read from the lock itself.
- cgroup files (`cpu.stat`, `cpu.max`, `memory.events`) show throttling and
  memory limits; `/proc/diskstats` and `/proc/*/io` show device load and who
  is causing it.

No kernel module, no tracing, no privileges beyond what `ps` and `cat` have.
The price is the list below.

## When it is wrong

- **It sees two seconds.** A stall that comes and goes can be missed. Use
  `-t 30`.
- **Idle and waiting can look alike.** A thread blocked reading a socket may be
  waiting for a slow server or may simply have nothing to do. wits calls a
  process with one connection *waiting* and an event loop with many
  connections *idle*, unless one of them has a request out with no reply.
- **Mutexes are recognised by convention.** glibc, Rust's standard library and
  the Go runtime park a contended-lock waiter the same way, and wits relies on
  that. The holder is named only for glibc mutexes. Other locks (the Python
  GIL, Go's `sync.Mutex`, `parking_lot`) show up as parked threads, not as
  `lock`.
- **Other users' processes are mostly opaque.** Without `sudo`, wits can see
  their CPU time, CPU waiting and disk waiting, but not which file, lock or
  socket. It tells you when this happens.
- **Ubuntu's default `ptrace_scope=1` hides detail** for processes that are not
  descendants of your shell: wits falls back to the kernel wait function and
  marks the answer low confidence. `sudo wits` sees everything.
- **Disk blame is partial without `sudo`:** I/O done by other users' processes
  is not visible, so wits may say "no visible culprit".
- **Memory needs evidence.** Without swap-ins or a cgroup limit being hit,
  pages read back from disk are reported as `disk`.
- **Virtual machines:** CPU time taken by the hypervisor is reported as
  `cpu-wait` with nobody to blame.
- **Containers:** processes and sockets in other namespaces cannot be named.
- **Not covered at all:** GPU waits, and anything inside the process that
  never reaches the kernel (a slow algorithm is just `cpu`).

Each case in [`cases/`](cases) starts one slow condition and states the answer
wits must give. `cases/run.sh` runs them all; it is the test suite.

## Requirements

Linux on x86_64 or aarch64. Best on kernel 5.16 or later; works on older
kernels with less detail when the system call is not readable.

## wits is finished

It does one thing. There is no roadmap: no new causes, no TUI, no daemon, no
macOS port. Bug reports are welcome and bugs get fixed.

## License

MIT
