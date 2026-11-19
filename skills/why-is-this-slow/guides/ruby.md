# Ruby: where the time usually goes

Read this when `code.lang` is `ruby`. It covers CRuby, and Rails where it
says so. Fixes are listed cheapest first. `general.md` covers what holds in
every language.

## See the program's functions

- Ruby 3.3 and later publish the methods YJIT has compiled when started with
  `ruby --yjit --yjit-perf`. Methods still in the interpreter stay hidden.
- Otherwise, and on macOS, wits shows the interpreter's C functions
  (`vm_exec_core`, `rb_ary_each`, `gc_mark`), which the hints read: they say
  what kind of work it is. For the Ruby line, use a Ruby profiler for that
  step: `rbspy dump --pid <pid>` (what it is doing now), `rbspy record`, or
  stackprof.

## Settings before code (ease 1)

- **YJIT.** `ruby --yjit` or `RUBY_YJIT_ENABLE=1` (Ruby 3.2 and later; Rails
  7.2 turns it on by itself on 3.3 and later). Often 15 to 40% faster on
  application code. It is the first thing to try when the hint is
  `interpreter`.
- **A newer Ruby.** Each release since 3.0 is faster, with or without YJIT.
- **Bootsnap** when the hint is `boot`: it caches load-path lookups and
  compiled code.
- **The collector.** A heap that grows faster is collected less often:
  `RUBY_GC_HEAP_GROWTH_FACTOR`, `RUBY_GC_MALLOC_LIMIT`,
  `RUBY_GC_OLDMALLOC_LIMIT`. `GC.stat` shows how many collections ran and
  why.
- **Memory of a threaded server.** glibc's allocator bloats under threads:
  `MALLOC_ARENA_MAX=2`, or jemalloc through `LD_PRELOAD`.
- **Workers and threads.** One process runs Ruby code on one core (the GVL).
  CPU-bound load needs more processes (Puma `workers`), not more threads;
  threads help while requests wait for I/O.

## cpu

| In the stack (`code.hints` id) | It means | What usually works |
| --- | --- | --- |
| `vm_exec_core`, `vm_call_*` (`interpreter`) | Ruby-level code is the cost | YJIT. Built-in methods instead of hand-written loops (`sum`, `tally`, `group_by`, `each_slice`); a `Hash` or `Set` instead of `Array#include?` or `find` in a loop; memoise (`@x ||=`); no `method_missing`, `send` or `define_method` on a hot path; `Struct` or `Data` instead of `OpenStruct` |
| `gc_mark*`, `gc_sweep*`, `newobj_*` (`gc`) | Many short-lived objects | `# frozen_string_literal: true`; `<<` and `map!` instead of `+` and `map` in loops; `each_with_object`; symbols or frozen constants for repeated strings; `pluck` instead of loading records |
| `onig_*`, `rb_reg_*` (`regex`) | Matching, or a regex compiled per evaluation | `match?`; no interpolation in a hot regex, or the `o` flag; `start_with?`, `include?` for fixed text |
| `rb_require*`, `rb_iseq_compile*` (`boot`) | Loading and compiling files | Bootsnap; fewer gems loaded at boot (`require: false`) |
| a gem, `frame.kind` `lib` | It is asked too often | JSON: parse once, a recent `json` or `oj`. Templates: cache fragments. Serialisers: fewer attributes, no query per attribute |

## peer (Rails and Active Record)

- **N+1 queries.** A query per record for an association: `includes`,
  `preload` or `eager_load`. The bullet gem, or `strict_loading`, finds them.
  Count the queries of one request in the log.
- **Loading records to read one column.** `pluck(:id)`, `select(:id, :name)`,
  `exists?` instead of `present?`, `count` in SQL instead of `to_a.size`.
- **Row by row.** `insert_all`, `upsert_all`, `update_all`, `delete_all`
  instead of a loop of `create`, `save` and `destroy`. `find_each` or
  `in_batches` for big sets, so they are not all in memory.
- **A slow query.** Ask wits about the database process. On `cpu` it is
  scanning: look for the missing index with `explain`.
- **HTTP calls.** One persistent connection (`Net::HTTP.start` with a block,
  or a client with a pool) instead of a new one per call. Calls to other
  services inside a request belong in a job when the user does not wait for
  the result.

## lock

- Threads waiting for the GVL usually show as idle to wits. A process with
  many threads and one core's worth of `cpu` is at its limit: add processes.
- The connection pool (`pool:` in `database.yml`) must cover the threads of
  each process, or threads wait for a connection.
- A file lock is usually Bundler, a migration, or two servers started on
  one pid file.

## disk, memory

- `File#flush` and `fsync` per line, or a log at `debug` level in
  production: the log is the workload. Raise the level.
- Memory that grows with each request: whole result sets loaded (`find_each`
  instead), strings built by `+=`, a cache with no bound.

## Measuring

- **Boot dominates short runs.** A rake task or one test file spends
  seconds loading Rails before it does anything. Measure the task's own part
  by comparing with an empty task, and fix boot with Bootsnap and fewer
  eager loads in development and test.
- Tests: `rspec --profile 10` or `rails test --profile` name the slow ones.
  The usual causes are records created with `create` where `build_stubbed`
  would do, a database cleaned by truncation instead of a transaction, and
  real sleeps or network calls.
- YJIT needs warm-up: a short run shows little gain from it; a server shows
  it after the first requests.
