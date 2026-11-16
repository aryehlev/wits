---
name: why-is-this-slow
description: Find out why a process is slow or stuck on Linux using the wits CLI. Use when a command, test run, build, server or script is taking much longer than expected, hangs, or makes no visible progress, and before guessing at causes such as CPU, disk, locks, memory or the network.
---

# Why is this slow?

When something on a Linux machine is slow or stuck, measure before you guess.
`wits` watches a process for two seconds and reports one cause, the specific
object involved, and which other process is to blame. It needs no root and
changes nothing.

## When to use it

- A command you started (tests, build, install, migration, script) runs far
  longer than expected or prints nothing.
- The user says something is slow, hanging, frozen or stuck.
- You are about to suggest "it might be CPU / disk / a lock / the network".
  Run wits first and say which one it is.

Do not use it for: a process that already exited, performance questions about
code that is not running, or non-Linux systems.

## How to run it

```sh
wits --json <pid|name>     # one process
wits --json                # the processes waiting most right now
```

- Use the pid when you have it (for a background command you started, `$!`).
  A name matches the command name first, then the command line.
- Watch longer for intermittent problems: `wits --json -t 10 <pid>`.
- If `wits` is not installed:
  `curl -fsSL https://raw.githubusercontent.com/aryehlev/wits/main/install.sh | sh`
  (installs one static binary to `~/.local/bin`), or `cargo install wits`.
- If the answer has `"confidence":"low"` or a note about a limited view, rerun
  with `sudo` if you are allowed to.

## Reading the answer

```json
{"pid":2574,"name":"deploy","state":"stalled","cause":"lock","share":1.0,
 "summary":"deploy (pid 2574) is waiting for a lock: 100% of its time over the last 2.0s",
 "what":"the file lock on /srv/app/schema.lock",
 "blame":{"kind":"process","pid":2571,"name":"migrate …/schema.lock","text":"held by pid 2571 (migrate …), which is only waiting for its child pid 2573 (psql)"},
 "breakdown":{"lock":1.0,"cpu":0.0,"cpu-wait":0.0,"disk":0.0,"memory":0.0,"peer":0.0},
 "idle":0.0,"confidence":"high","followed":[],"notes":[]}
```

| Field | Meaning |
| --- | --- |
| `summary` | The answer in one sentence. Quote it to the user. |
| `state` | `working` (on CPU), `stalled` (held back by the machine), `waiting` (on another process or the network), `idle` (not slow: nothing to do), `stopped`, `unknown` (blocked, but not visible) |
| `cause` | One of `cpu`, `cpu-wait`, `disk`, `memory`, `lock`, `peer`, or `null` when idle |
| `share` | Fraction of the process's non-idle time spent on that cause |
| `what` | The specific object: file, device, lock, socket, address |
| `blame` | Who is responsible. `kind` is `self`, `process`, `thread`, `cgroup`, `remote`, `device` or `unknown`; `pid` is set when it is a process or thread |
| `breakdown` | Share of non-idle time for all six causes |
| `followed` | Parents wits skipped because they were only waiting for this child |
| `confidence`, `notes` | How far to trust it, and why |

## What to do next

| `cause` | It means | Next step |
| --- | --- | --- |
| `cpu` | The process is computing; nothing is in its way | It is the code or the input size. Profile it (`perf top -p`, `py-spy dump`) or wait. Do not look for an external culprit. |
| `cpu-wait` | It wants a CPU and cannot get one | `blame.kind` `process`: another process is hogging the CPU; tell the user which. `cgroup`: a container CPU limit is throttling it; raise the limit or lower parallelism. |
| `disk` | Blocked on storage | `what` names the file and device. If `blame` is another process, that one is saturating the disk. If it is `self`, the workload is I/O bound (often fsync). |
| `memory` | Swapping or evicting its own pages | `blame.kind` `cgroup`: the memory limit is too small. Otherwise the named process is using the RAM. |
| `lock` | Queued on a file lock or mutex | `blame.pid` holds it. Run `wits --json <blame.pid>` to see why the holder is not releasing. Never kill the holder without asking the user. |
| `peer` | Waiting for another process or the network | `blame.kind` `process`: run `wits --json <blame.pid>` on it and repeat until you reach a process that is not just waiting. `remote`: the delay is on another machine; `what` has the address. |
| `null` + `idle` | Not slow. It is asleep, waiting for input, or has no work | Check whether it is waiting for something the user must provide (stdin, a prompt). `idle_what` says what it is waiting on. |

Follow the chain: when wits blames another process, ask wits about that
process. Stop when you reach `cpu`, `disk`, `memory`, a `cgroup`, a `remote`
host or an idle process. That last one is the real answer.

## Limits to keep in mind

- It samples two seconds. An intermittent stall may need `-t 30`.
- Without `sudo` it cannot see inside other users' processes.
- Locks that never reach the kernel in the usual way (the Python GIL, Go's
  `sync.Mutex`) are not reported as `lock`.
- It never changes or signals any process. Any fix is your decision and the
  user's.
