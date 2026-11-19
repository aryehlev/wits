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
wits guide <language>                # what usually fixes it in that language (see below)
wits on <where> --json <pid|name>    # the same on another machine (see "On another machine")
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
  `wits_explain`, `wits_overview`, `wits_profile`, `wits_run`, `wits_diff`,
  `wits_remote` for another machine, and `wits_guide` for the guides.
- Before fixing, rank the candidate fixes by 0.7 × gain + 0.3 × ease (see
  "Choose the fix" below) and start with the highest.
- To show the user, add `--html <file>`: a web page with the answer and a
  flame graph per cause (or `html_file` with the MCP tools).

## On another machine

When the slow process is on a server or a cloud VM, do not guess from here
and do not install anything there by hand: `wits on <where>` runs wits on
that machine and brings the same answer back. What follows `<where>` is what
you would type there.

```sh
wits on deploy@web-1 --json api            # any machine you can ssh to (names from ~/.ssh/config work)
wits on gcp:web-1 --json api               # a Google Cloud VM by name: gcp:[project/][zone/]instance
wits on aws:web-1 --json api               # an EC2 instance by Name tag or id: aws:[user@][region/]<id>
wits on azure:shop-rg/web-1 --json api     # an Azure VM: azure:[user@]<resource group>/<vm>
wits on --sudo web-1 --json                # what is waiting most there, seen as root
wits on web-1 --html page.html --json api  # the page is written here
```

- Use the way the user already has to that machine: an ssh name from their
  config, or the cloud CLI they are logged in to. Do not create keys, open
  ports or change firewall rules to get in; if the login fails, show the
  user the error.
- Most server processes belong to another user. If the answer says the
  process cannot be seen, or has `"confidence":"low"`, rerun with `--sudo`
  (before `<where>`): it uses `sudo -n` and fails if sudo wants a password.
- The first time, wits copies its binary (a few megabytes) into `~/.cache/wits` on
  that machine and says so on stderr. It changes nothing else there. Add
  `--rm` if the user wants nothing left behind.
- Follow blame across machines: when `blame.kind` is `remote` and you can
  reach that host, run `wits on <that host> --json <the server's name>`,
  until the answer is `cpu`, `disk`, `memory`, a limit or an idle process.
- `wits on <where> run -- <command>` runs the command on that machine. Only
  do that with a command the user asked you to run there.
- `wits diff` and `wits guide` work here, on files and text: no `on`.
- It cannot reach what has no login: a managed database, Lambda, Cloud Run.
  Those appear as the `peer` a process waits for, with the address; say so
  and look at that service's own metrics. Kubernetes pods: `kubectl wits`.

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
 "code":{"source":"ebpf+stack-scan","lang":"c","notes":[],"causes":[
   {"cause":"disk","seconds":1.94,"hot":[
     {"share":1.0,"seconds":1.94,
      "frame":{"func":"flush_log","module":"server","file":"/srv/app/log.c","line":41},
      "leaf":{"func":"fsync","module":"libc.so.6","file":null,"line":null},
      "callers":[{"func":"handle_request","module":"server","file":"/srv/app/server.c","line":88}]}]}],
   "hints":[]}}
```

And `code.hints` of a Go server that is on CPU:

```json
"lang":"go",
"hints":[
 {"cause":"cpu","share":0.42,"seconds":1.05,"id":"alloc-gc","lang":"go","seen":"runtime.scanobject",
  "means":"allocating memory and collecting garbage",
  "try":"Allocate less on the hot path: … As a setting, GOGC=200 or more trades memory for fewer collections; …"},
 {"cause":"cpu","share":0.26,"seconds":0.64,"id":"json","lang":"go","seen":"encoding/json.stateInString",
  "means":"encoding/json: JSON encoding and decoding driven by reflection","try":"…"}]
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
| `code.lang` | The program's language, when the stacks tell: `python`, `node`, `go`, `rust`, `jvm`, `dotnet`, `ruby`, `beam` (Erlang, Elixir), `c` (C, C++), or `null`. Read the guide for it (below). |
| `code.hints` | Costs wits knows by name, summed over every stack, largest first: allocation and garbage collection, the interpreter's own loop, regex, JSON, reflection, a contended lock, a debug build. `cause` and `share`: the part of that cause's time spent in it. `seen`: the function most of it was seen in. `means` and `try`: what it is and what usually fixes it. `"whole": true` is a sign about the whole program (a debug build, a sanitizer): fix that before anything else. `[]` when wits recognised nothing. |
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

## Know the language

wits says where the time goes. What to change depends on the language, and
the same few causes come back in each one. Before you choose a fix, read the
guide for `code.lang`, and `general` for what holds everywhere:

| `code.lang` | Guide | Covers |
| --- | --- | --- |
| `python` | [guides/python.md](guides/python.md) | CPython: the interpreter loop, the GIL, imports, ORMs, asyncio |
| `node` | [guides/node.md](guides/node.md) | Node.js, JavaScript, TypeScript: the one thread, V8's collector, sequential awaits |
| `go` | [guides/go.md](guides/go.md) | Allocation and GC, `GOGC`, `GOMAXPROCS` in containers, goroutines wits cannot see |
| `rust` | [guides/rust.md](guides/rust.md) | Release builds, allocation, hashing, async runtimes |
| `jvm` | [guides/jvm.md](guides/jvm.md) | Java, Kotlin, Scala: heap and collector, warm-up, ORMs, build daemons |
| `dotnet` | [guides/dotnet.md](guides/dotnet.md) | C#, F#: Release builds, GC, LINQ, blocking on async code |
| `ruby` | [guides/ruby.md](guides/ruby.md) | CRuby and Rails: YJIT, GC, N+1 queries, boot time |
| `beam` | [guides/beam.md](guides/beam.md) | Erlang, Elixir: schedulers, mailboxes wits cannot see, ETS |
| `c` | [guides/c.md](guides/c.md) | C and C++: optimisation flags, allocators, containers, iostreams |
| any | [guides/general.md](guides/general.md) | Each cause in any language: databases and N+1 queries, networks, disks, containers, builds and tests, and when a measurement misleads |

Without the files, `wits guide <language>` prints the same text, and the MCP
tool `wits_guide` returns it. `wits guide` alone lists the topics.

Each guide has, in this order: how to make the runtime show the program's own
functions; the settings to try before changing code; a table from what is in
the stack (and each `code.hints` id) to what it means and what usually works;
the usual `lock`, `peer`, `disk` and `memory` problems; and what misleads when
measuring.

How to use a hint and a guide together:

1. A hint with `"whole": true` comes first. A debug build, a sanitizer or a
   race detector makes every other number about a different program.
2. A hint is a measured share, so it bounds the gain: at most
   `breakdown[hint.cause] × hint.share` of the time.
3. Hints overlap each other and the hot spots: JSON decoding that allocates is
   in both `json` and `alloc-gc`. Do not add their shares.
4. The hot spot says where to change the code; the hint says what kind of
   work it is; the guide says what usually helps and which setting to try
   first. A hint without a matching hot spot (allocation spread over the whole
   program) is fixed by a setting or at the few call sites that allocate most.
5. `try` and the guides say what usually works, not what will. Measure each
   change with `wits run` and `wits diff`.

## Choose the fix: most gain for the least effort

Before changing code, list the candidate fixes and score each one:

**score = 0.7 × gain + 0.3 × ease**

- **gain** (0 to 1): the share of the measured time the change would remove.
  wits gives the upper bound: the cause's `share` times the hot spot's
  `share` in `code.causes` (for a run report, `breakdown[cause]` times the hot
  spot's `share`), or times a hint's `share` in `code.hints`. A fix that
  removes half of a hot spot gets half of that.
- **ease** (0 to 1): **1** a flag, a config value or one line; **0.7** a small
  change inside one function; **0.4** several functions or files, or an API
  change; **0.1** a redesign, a new dependency or a different architecture.

The guides list settings first because they are ease 1: an environment
variable, a build flag, a runtime option. Put the ones that match the cause
among the candidates even when a code change would gain more.

Make the highest-scoring change first, one change at a time, and measure each
one (below). Tell the user the scores you estimated next to what `wits diff`
measured. If the diff says `no clear change`, undo the change unless it is
wanted for another reason: effort that buys no speed is not worth keeping.

For example, `disk` is 96% of the time and 100% of it is `fsync` in `flush_log`:

| Candidate | gain | ease | score |
| --- | --- | --- | --- |
| fsync once per batch of writes, not per write | 0.85 | 0.7 | **0.81** |
| rewrite the storage layer | 0.95 | 0.1 | 0.70 |
| move the file to a faster disk | 0.5 | 0.4 | 0.47 |

Start with the batch: almost all of the gain for a fraction of the work.

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
- The saved report has `functions`: every function's `seconds` for each
  `cause`, all of its lines added up. `code.causes[].hot` is only the hottest
  lines; `wits diff` compares `functions`.
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
- `code.hints` holds the costs wits knows by name, not every cost: an empty
  list does not mean the code is fine. `code.lang` is `null` when no frame
  tells the language (a stripped binary, stacks that stop early).
- wits watches threads. Goroutines, async tasks and Erlang processes that
  wait hold no thread: their waits show as idle threads or as `peer` on a
  connection, not as a stack in your handler. The guides name each runtime's
  own tool for that.
- A process on another node is only named when that node runs the wits agent.
  On another machine that you can log in to, ask with `wits on`.
- It never changes or signals any process. Any fix is your decision and the
  user's.
