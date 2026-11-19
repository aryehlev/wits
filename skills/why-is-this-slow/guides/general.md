# Any language: what each cause usually comes down to

Read this with the answer in front of you. It holds for every language; the
language guides add what is particular to each. Fixes are listed cheapest
first, so the first one that applies usually has the best score.

## Before the code: is the measurement honest?

- **A build made for debugging.** A dev or Debug build, a sanitizer, a race
  detector, coverage, a debugger or a profiler attached: each slows a program
  2 to 50 times and moves where the time goes. wits says so when it sees one
  (`code.hints` with `"whole": true`, or a `tracing` hint). Measure the
  optimised build.
- **A short run is mostly start-up.** Under about a second, an interpreter or
  JIT spends its time loading and compiling code. The fix is to load less or
  keep a warm process (a daemon, a server, a watch mode), not to tune the
  loop.
- **Caches change the second run.** The first run reads files from disk and
  the next ones from memory; `go test`, build tools and package managers cache
  results. `wits run -n 5` takes the median: check `runs_ms` for a first run
  that stands apart, and turn result caches off for the measurement
  (`go test -count=1`).
- **Emulation.** An amd64 container on an arm64 machine (or the reverse) runs
  under QEMU or Rosetta, several times slower, and all of it shows as `cpu`.
  Compare `uname -m` with the image's architecture.
- **Whose time is it?** If `blame` names another process, a cgroup or a remote
  host, the code in `where` is waiting, not slow. Follow the blame first.

## cpu: the program is computing

Find out whether the work is needed before making it faster.

1. **Work repeated for nothing.** The same value computed, parsed, compiled
   or fetched once per item instead of once: a regex compiled in the loop, a
   file or config read again per call, a sort inside a loop, data serialised
   and parsed again between two layers of the same program. Hoist it or cache
   it. Ease 0.7 to 1.
2. **Accidentally quadratic.** Searching a list inside a loop over it (use a
   hash set or map), appending to a string or array by copying it each time,
   removing from the front of an array, nested loops over the same data that
   one pass with a map would replace. It hurts only at size, so test with
   real input sizes. Ease 0.7.
3. **Work nobody uses.** Log and debug lines whose text is built although the
   level is off, checks and assertions meant for development, copies of data
   that is only read, results computed and thrown away.
4. **Per item instead of per batch.** A function call, an allocation, a lock,
   a system call or a query for each small item. Do it once for many.
5. **Only then: more cores.** Parallelism helps when the cause is `cpu`, the
   work splits, and CPUs are free. If `cpu-wait` is already there, more
   threads make it worse.

`frame.kind` says where to act. `app`: change that code. `lib`: the package
named in `frame.package` is doing what it was asked; ask for less, less often,
or through a cheaper call. `std` or `sys` as the `leaf`: the runtime is busy
on the program's behalf, and `code.hints` usually says with what.

## cpu-wait: wants a CPU, cannot get one

- `blame.kind` `process`: something else is using the CPUs. Tell the user
  which; do not tune the victim.
- `blame.kind` `cgroup`: a container CPU limit is throttling it. Most
  runtimes size their thread pools from the machine's cores, not the limit:
  set the worker count to the limit (`GOMAXPROCS`, `OMP_NUM_THREADS`,
  `-XX:ActiveProcessorCount`, `+S`, `--workers`, `-j`), or raise the limit.
  Ease 1.
- If the process's own threads are the ones competing (more runnable threads
  than CPUs), use fewer workers: they finish sooner.

## disk: blocked on storage

- **fsync per write.** `what` shows `fsync()` or `fdatasync()`: the program
  makes each small write durable on its own. Sync once per batch (group
  commit), or wrap many database statements in one transaction. For SQLite:
  one transaction around the inserts, `PRAGMA journal_mode=WAL` and
  `synchronous=NORMAL`.
- **Many small reads or writes.** Buffer them; read a file once, not once
  per record.
- **A slow place to keep files.** A network file system, or a directory
  bind-mounted into Docker Desktop on macOS or Windows (`node_modules`, build
  output): move it to a volume or the container's own disk.
- **Reading back its own pages.** Without evidence of memory pressure, wits
  reports pages read back from disk as `disk`. If the file was just read,
  suspect a memory limit.
- If `blame` names another process, that one is saturating the device.

## memory: swapping or evicting

- `blame.kind` `cgroup`: the memory limit is too small for what the program
  holds. Hold less (stream instead of loading everything, smaller batches,
  smaller caches) or raise the limit. Runtimes with a garbage collector need
  room above the live data: tell them the limit (`-XX:MaxRAMPercentage`,
  `GOMEMLIMIT`, `--max-old-space-size`).
- Otherwise the process named in `blame` holds the RAM.
- A program that slows down the longer it runs is growing: a leak, or a
  cache with no bound.

## lock: queued behind a holder

- **A file lock.** `what` names the file. Package managers, build
  directories, `git` (`index.lock`), SQLite and state files allow one writer:
  two commands were started at once, or an earlier one is still running or
  hung. `blame.pid` holds it: ask wits about that pid. Never kill it without
  asking the user.
- **A mutex.** `where` is the code waiting; the holder is doing something
  slow inside the lock. Do less while holding it (no I/O, no logging, no
  allocation), split it, or avoid sharing.
- A lock inside a database (rows, tables) shows as `peer`: the client waits
  for the server.

## peer: waiting for another process or the network

Ask what is at the other end, then why it takes long.

1. **Many small round trips.** One query or request per item, each waiting
   for the reply (the N+1 pattern): the time is in the waits, and neither
   side is busy. Ask once for everything: a join or `IN` list, a bulk or
   batch endpoint, a pipeline. This is the most common cause. Ease 0.4 to
   0.7.
2. **One after another.** Independent requests sent in series. Send them
   together, with a bound on how many at once.
3. **A new connection each time.** A TCP and TLS handshake per request costs
   several round trips. Reuse connections: one shared client, a pool,
   keep-alive.
4. **Round numbers.** A wait of exactly 1, 3, 5 or 30 seconds is a timeout or
   a retry, not slow work: something failed first. A stall of about 40 ms on
   small request-reply messages is Nagle's algorithm meeting delayed ACKs:
   set `TCP_NODELAY`, or send the message in one write. A stall of 5 seconds
   on a lookup is DNS.
5. **DNS.** A lookup per request, a slow resolver, or in Kubernetes the
   search list tried first for every outside name (`ndots:5`): end the name
   with a dot, or cache.
6. **The other side really is slow.** `blame.kind` `process`: ask wits about
   it. A database on `cpu` is scanning (a missing index: look at the query
   plan); on `disk` it is syncing or reading cold data; on `lock` it waits
   for a transaction that stays open too long.
7. **Waiting for a turn.** A connection pool with no free connection, a rate
   limit, a queue. Threads waiting for the pool may show as `lock` or idle.
   Shorter transactions, returned connections, or a larger pool.

## idle, but the user says it is slow

- It waits for input: a prompt, a password, a `[y/N]`, an editor that was
  opened. `idle_what` says what it waits on.
- It sleeps: a retry with backoff, a poll with `sleep`, a test that waits a
  fixed time instead of for the condition.
- An event loop or async runtime (Node, Go, asyncio, Tokio, the BEAM) parks
  its tasks without holding a thread, so a task waiting for a reply may show
  as idle. Look at the connections in `what`, and at the runtime's own
  tools.

## Builds, tests and installs

These are trees of processes. `wits run` shows the time per process and per
cause for the whole tree: read that before guessing.

- **Not using the cores.** `make -j`, `pytest -n auto`, `go test -p`,
  `jest --maxWorkers`, `ctest -j`, `mvn -T 1C`, `gradle --parallel`. Ease 1.
- **Rebuilding what has not changed.** A cache that never hits: a clean
  checkout every time, a Docker layer that copies the source before the
  dependencies are installed, a cache directory that is not kept between
  runs. A compiler cache (ccache, sccache) helps C, C++ and Rust.
- **Compiling dependencies from source.** A package without a prebuilt
  binary for this platform (Python wheels on Alpine's musl, for one) is
  compiled at install time: minutes of `cpu` in a compiler. Use a platform
  that has binaries.
- **Linking.** A long single-threaded `ld` at the end: a faster linker (mold,
  lld) is a flag.
- **A few slow tests.** Ask the test runner which (`pytest --durations=10`,
  `go test -json`, `jest --verbose`), then ask wits why: usually a `sleep`,
  a real network call, or a database set up again for each test.
- **Installs.** `peer` is the network (a registry, a slow mirror), `cpu` is
  compiling native code, `disk` is unpacking many small files.
