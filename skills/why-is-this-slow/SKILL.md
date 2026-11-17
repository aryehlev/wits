---
name: why-is-this-slow
description: Find out why a process is slow or stuck, and where in its code the time goes, using the wits CLI (Linux and macOS, including Kubernetes pods). Use when a command, test run, build, server or script takes much longer than expected, hangs, or makes no visible progress, before guessing at causes such as CPU, disk, locks, memory or the network; and to check whether a performance change really made something faster.
---

# Why is this slow?

When something is slow or stuck, measure before you guess. `wits` watches a
process for two seconds and reports one cause, the specific object involved,
which other process is to blame, and the function and line where the time
goes. It needs no root and changes nothing. It works on Linux and macOS.

## When to use it

- A command you started (tests, build, install, migration, script) runs far
  longer than expected or prints nothing.
- The user says something is slow, hanging, frozen or stuck.
- You are about to suggest "it might be CPU / disk / a lock / the network".
  Run wits first and say which one it is.
- You changed code to make it faster. Measure before and after with
  `wits run` and `wits diff` instead of claiming a speedup.

Do not use it for a process that has already exited, or for code that is not
running (use `wits run` to run it).

## How to run it

```sh
wits --json <pid|name>               # one process: cause, object, blame, where in the code
wits --json                          # the processes waiting most right now
wits --json <target> <target>...     # several at once, busiest first
wits --json <namespace>/<pod>        # (Linux, on the node) each process of a pod
wits --json <namespace>/<deployment> # (Linux) every replica of a Deployment, StatefulSet, DaemonSet or Job
wits run -n 5 -o before.json -- <command...>   # run a command; where did its time go?
wits diff --json before.json after.json        # did the change help?
```

- Use the pid when you have it (for a background command you started, `$!`).
  A name matches the command name first, then the command line.
- Watch longer for intermittent problems: `wits --json -t 10 <pid>`.
- If `wits` is not installed:
  `curl -fsSL https://raw.githubusercontent.com/aryehlev/wits/main/install.sh | sh`
  (one static binary in `~/.local/bin`), or `cargo install wits`.
- If the answer has `"confidence":"low"` or a note about a limited view, rerun
  with `sudo` if you are allowed to. On Linux, `sudo` also adds on-CPU stacks
  (eBPF).
- In Kubernetes, from outside the node: `kubectl wits <namespace>/<pod> --json`,
  or `kubectl wits -n <namespace> deploy/<name> --json` for every replica.
  Without the wits DaemonSet it starts short-lived privileged pods on the
  nodes involved; that needs permission to create privileged pods.
- If the wits MCP server is connected (`wits mcp`), its tools do the same:
  `wits_explain`, `wits_overview`, `wits_profile`, `wits_run`, `wits_diff`.
- To show the user, add `--html <file>`: a web page with the answer and a
  flame graph per cause (or `html_file` with the MCP tools).

## Reading the answer

Abridged:

```json
{"pid":4148,"name":"fs","pod":null,"state":"stalled","cause":"disk","share":0.97,
 "summary":"fs (pid 4148) is waiting for disk: 97% of its time over the last 2.0s",
 "what":"fsync() on /srv/app/log.bin, device sda1 (100% busy)",
 "where":"100% in flush_log (…/app/log.c:41) ← handle_request (…/app/server.c:88)",
 "blame":{"kind":"self","pid":null,"name":null,"node":null,"pod":null,
          "text":"its own I/O: it read 0 B, wrote 5.3 MB; no other visible process is doing more"},
 "breakdown":{"disk":0.97,"cpu":0.03,"cpu-wait":0.0,"memory":0.0,"lock":0.0,"peer":0.0},
 "idle":0.0,"confidence":"high","followed":[],"notes":[],
 "code":{"source":"ebpf+stack-scan","notes":[],"causes":[
   {"cause":"disk","seconds":1.94,"hot":[
     {"share":1.0,"seconds":1.94,
      "frame":{"func":"flush_log","module":"server","file":"/srv/app/log.c","line":41},
      "leaf":{"func":"fsync","module":"libc.so.6","file":null,"line":null},
      "callers":[{"func":"handle_request","module":"server","file":"/srv/app/server.c","line":88}]}]}]}}
```

| Field | Meaning |
| --- | --- |
| `summary` | The answer in one sentence. Quote it to the user. |
| `state` | `working` (on CPU), `stalled` (held back by the machine), `waiting` (on another process or the network), `idle` (not slow: nothing to do), `stopped`, `unknown` (blocked, but not visible) |
| `cause` | One of `cpu`, `cpu-wait`, `disk`, `memory`, `lock`, `peer`, or `null` when idle |
| `share` | Fraction of the process's non-idle time spent on that cause |
| `what` | The specific object: file, device, lock, socket, address |
| `where` | The function, file and line responsible for `cause`, and its caller. Missing when no stacks were collected. |
| `blame` | Who is responsible. `kind` is `self`, `process`, `thread`, `cgroup`, `remote`, `device` or `unknown`; `pid` is set when it is a process or thread; `pod` and `node` say where it lives |
| `blame.upstream` | When the other end of a connection is on another node with a wits agent: `node`, and `answer`, that node's full answer for the blamed process (same shape as this one). Read it instead of rerunning. |
| `blame.conn` | `local` and `remote` address of the connection being waited on |
| `breakdown` | Share of non-idle time for all six causes |
| `code.causes` | Per cause, the hottest functions: `frame` (the first function in the program's own code), `via` (the library function it called on the way), `leaf` (where the time actually is, e.g. `fsync`), `callers`, `share`, `seconds` |
| `frame.kind`, `frame.package` | Whose code a frame is: `app` (the program's own), `lib` (a dependency, named in `package`: a crate, Go module, npm, Python or Ruby package, or shared library), `std` (the language's standard library and runtime), `sys` (the C library), `kernel`. Change `app` code; a `lib` hot spot usually means the program calls it too often or with too much data. |
| `code.source`, `code.notes` | How stacks were collected (`ebpf`, `stack-scan`, `sample`), and what is missing and why |
| `pod` | `namespace/pod/container` when the process runs in Kubernetes, else `null` |
| `followed` | Parents wits skipped because they were only waiting for this child |
| `confidence`, `notes` | How far to trust it, and why |

Several processes (several targets, a pod with more than one process, or a
workload) give `"kind":"group"` instead: `summary`, and `items`, busiest
first, each with `node` and `answer` (the shape above). Start with
`items[0].answer`; compare the others to see whether all replicas have the
same problem or one is different.

`wits profile <pid>` prints the same stacks as text, every cause with its call
paths, for showing to the user. `wits profile --folded <pid>` gives folded
stacks for `flamegraph.pl`.

## What to do next

| `cause` | It means | Next step |
| --- | --- | --- |
| `cpu` | The process is computing; nothing is in its way | It is the code or the input size. Open the file and line in `where` and in `code.causes[cause=cpu].hot`; that is the code to make faster. Do not look for an external culprit. |
| `cpu-wait` | It wants a CPU and cannot get one | `blame.kind` `process`: another process is hogging the CPU; tell the user which. `cgroup`: a container CPU limit is throttling it; raise the limit or lower parallelism. |
| `disk` | Blocked on storage | `what` names the file and device, `where` the code doing it (often an `fsync` per write). If `blame` is another process, that one is saturating the disk. If it is `self`, the workload is I/O bound: batch writes, fsync less. |
| `memory` | Swapping or evicting its own pages | `blame.kind` `cgroup`: the memory limit is too small. Otherwise the named process is using the RAM. |
| `lock` | Queued on a file lock or mutex | `where` is the code taking the lock. `blame.pid` holds it: run `wits --json <blame.pid>` to see why the holder is not releasing. Never kill the holder without asking the user. |
| `peer` | Waiting for another process or the network | `blame.kind` `process`: run `wits --json <blame.pid>` on it (or read `blame.upstream.answer`) and repeat until you reach a process that is not just waiting. `remote`: the delay is on another machine; `what` has the address. |
| `null` + `idle` | Not slow. It is asleep, waiting for input, or has no work | Check whether it is waiting for something the user must provide (stdin, a prompt). `idle_what` says what it is waiting on. |

Follow the chain: when wits blames another process, ask wits about that
process. Stop when you reach `cpu`, `disk`, `memory`, a `cgroup`, a `remote`
host or an idle process. That last one is the real answer.

If `code.notes` says a runtime's functions are hidden (Node, Python, Java,
.NET, Ruby, Erlang), it names the switch that makes the runtime publish them,
for example `node --perf-basic-prof` or `PYTHONPERFSUPPORT=1`. `wits run` sets
the environment-variable ones itself. Restarting a running service with a
switch is the user's decision.

## Check a fix

```sh
wits run -n 5 -o before.json -- <command...>
# change the code
wits run -n 5 -o after.json -- <command...>
wits diff --json before.json after.json
```

- `wits run` runs the command, watches it and everything it starts until it
  exits, and saves the report with `-o`. Its exit status is the command's.
  Its own report goes to stderr, so the command's output is untouched.
- `-n 5` runs the command five times and uses the median; it stops at the
  first failing run. Only repeat commands that are safe to run again (tests,
  builds, benchmarks), never a migration or a deploy. Use the same command and
  input before and after.
- The diff has `verdict`: `faster`, `slower` or `no clear change`;
  `wall_ms.before`, `wall_ms.after` and `wall_ms.change` (a fraction, −0.68 is
  68% faster); `noise`, which says whether the runs overlap; `causes`, seconds
  per cause before and after; and `functions`, the functions whose time
  changed most.
- Report the verdict as it is. `no clear change` means the difference is within
  run-to-run noise: do not call it a speedup. With fewer than 3 runs on either
  side, wits can only apply a 5% threshold, and `noise` tells you to repeat
  with `-n 5`.

## Limits to keep in mind

- It samples two seconds. An intermittent stall may need `-t 30`.
- Without `sudo` it cannot see inside other users' processes.
- Without root on Linux there are no stacks for time on CPU, only for blocked
  threads; a note says so.
- Stacks need frame pointers in native code; without them `where` may be short
  or missing.
- On macOS, the holder of a mutex is a best guess and says "probably"; in
  `wits run`, processes that live under 60 ms have no stacks, and short runs
  undercount function time.
- Python before 3.12 shows interpreter functions, not the program's.
- A process on another node is only named when that node runs the wits agent.
- It never changes or signals any process. Any fix is your decision and the
  user's.
