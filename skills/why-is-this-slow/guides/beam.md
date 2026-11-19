# Erlang and Elixir (the BEAM): where the time usually goes

Read this when `code.lang` is `beam`. Fixes are listed cheapest first.
`general.md` covers what holds in every language.

## See the program's functions

- Start the VM with `ERL_FLAGS='+JPperf true'` (`wits run` sets it; it works
  for `elixir`, `mix` and `iex` too). On Linux, with the JIT (OTP 24 and
  later on x86-64, OTP 25 on arm64), wits then shows Erlang and Elixir
  functions as `$module:function/arity`.
- Without it, and on macOS, wits shows the emulator's own C functions
  (`erts_garbage_collect`, `copy_struct`, `db_*`), which the hints read.

## What wits can and cannot see here

wits watches threads: the BEAM's schedulers. An Erlang process is not a
thread. A process waiting for a message holds no scheduler, so:

- A mailbox that grows, or every caller waiting on one busy `GenServer`,
  does **not** show as `lock` or `peer`. It shows as one scheduler in `cpu`
  (the busy process) while the node is otherwise idle, or as nothing at all.
- For that, ask the VM itself: `:observer.start()`, `:msacc` (where
  scheduler time goes), and with the recon library
  `:recon.proc_count(:message_queue_len, 5)` (the longest mailboxes) and
  `:recon.proc_window(:reductions, 5, 1000)` (the busiest processes).

What wits does show well: scheduler threads on `cpu` and in which emulator
function, `cpu-wait` under a container limit, `disk` and `peer` waits of the
dirty I/O schedulers and of ports, and memory pressure.

## Settings before code (ease 1)

- **Busy waiting** (hint `busy-wait`): `+sbwt none +sbwtdcpu none +sbwtdio
  none` stops idle schedulers spinning. It lowers CPU use, not latency.
- **Schedulers in a container.** OTP 23 and later size them from the CPU
  quota. Older releases start one per core of the host: set `+S <n>`.
- **Dirty I/O schedulers** (`+SDio`, default 10) carry file operations. With
  heavy file I/O, more of them help; `disk` waits on those threads are the
  sign.
- **`MIX_ENV=prod`** and a release for anything measured: `dev` recompiles,
  reloads code and logs more.
- **Logger level.** A log line per message is often the workload. When its
  queue is long, Logger makes callers wait.

## cpu

| In the stack (`code.hints` id) | It means | What usually works |
| --- | --- | --- |
| `erts_garbage_collect`, `do_minor`, `major_collection` (`gc`) | A process with a big heap collecting often | Keep big state out of busy processes (ETS, `:persistent_term`); `min_heap_size` for short-lived workers that allocate a lot; iodata instead of binary concatenation; `:hibernate` for idle processes that hold memory |
| `copy_struct`, `size_object` (`copying`) | Terms copied: messages, ETS reads and writes | Send a key or a reference, not the data; only the fields needed; `:ets.lookup_element`; `:persistent_term` for big read-mostly data |
| `db_*` (`ets`) | ETS operations | Lookups by key; `select` and `match` without the key scan the table; `read_concurrency`, `write_concurrency`; `:ets.update_counter` |
| `scheduler_wait` (`busy-wait`) | Schedulers spinning, not working | The `+sbwt` flags above |
| `$module:function/arity`, `frame.kind` `app` | Your function | Below |

In your own functions:

- `list ++ [x]` in a loop or a reduce is quadratic: prepend and reverse, or
  build iodata.
- `length(list)`, `Enum.at/2`, `List.last/1` walk the list: do not call them
  per iteration. Keep a count, or use a map or a tuple.
- Chains of `Enum` calls build a list at each step: one `Enum.reduce`, a
  comprehension, or `Stream` for large inputs.
- Strings: build iodata (`[a, ", ", b]`) and hand it to the socket or file
  as it is, instead of concatenating binaries.
- A `receive` that matches selectively on a long mailbox scans the mailbox
  each time.
- A NIF that runs longer than a millisecond blocks a scheduler: make it a
  dirty NIF.
- JSON: `Jason`, or the built-in `:json` module (OTP 27).

## One process as the bottleneck

Every `GenServer.call` to one process is served one at a time. If callers
queue, wits shows little; the mailbox length shows it. Fixes, cheapest
first: do the slow work outside the server (reply with `GenServer.reply`
from a task); let callers read from ETS (`read_concurrency: true`) or
`:persistent_term` instead of asking the server; split the server by key
(`PartitionSupervisor`, a `Registry`); or a pool.

## peer

- **Ecto N+1.** `Repo.preload` or a `join` with `preload` in the query,
  instead of a query per row. `Repo.insert_all` for many rows.
- **The connection pool** (`pool_size`): "connection not available" errors
  and queue-time warnings mean requests wait for a connection: a larger
  pool, or shorter transactions.
- **One after another.** `Task.async_stream(items, fun, max_concurrency: n)`
  for independent calls.
- **Between nodes**, a `busy_dist_port` (see `:erlang.system_monitor`) means
  the distribution buffer is full: raise `+zdbbl`, or send less.

## Measuring

- `mix test` and `mix run` compile first: run `mix compile` before, so that
  `wits run` measures the work and not the compiler.
- For a function, Benchee measures more exactly than timing a whole run,
  much of which is the VM starting and stopping.
- Measure a release, or at least `MIX_ENV=prod`.
