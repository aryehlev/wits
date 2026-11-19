# C and C++: where the time usually goes

Read this when `code.lang` is `c` (C or C++). Fixes are listed cheapest
first. `general.md` covers what holds in every language.

## First: is it an optimised build?

- No `-O` flag means `-O0`: several times slower, and its hot spots are not
  those of the optimised program. **CMake without `CMAKE_BUILD_TYPE` passes
  no optimisation flag at all**: configure with
  `-DCMAKE_BUILD_TYPE=Release` (or `RelWithDebInfo`).
- Not under a sanitizer (`-fsanitize=…`), valgrind, or libstdc++'s debug
  mode (`_GLIBCXX_DEBUG`). wits says so when it sees them on the stack
  (hints `sanitizer`, `debug-containers`).
- `assert` is still compiled in unless `-DNDEBUG` is given. An expensive
  assertion in a hot loop is a common surprise.

## See the program's functions

- Whole stacks need frame pointers: `-fno-omit-frame-pointer` (add
  `-mno-omit-leaf-frame-pointer` for the last frame). wits says so when
  stacks stop early, and names the library responsible. Distribution
  libraries built without them cut the stack below their callers.
- File and line need `-g` (or `-g1`, `-gline-tables-only`). It costs no
  speed. Do not strip the binary you measure.

## Settings before code (ease 1)

- **`-O2` or `-O3`**, and `-march=native` when the program runs where it is
  built (or a level such as `-march=x86-64-v3`): wider vectors for numeric
  loops.
- **Link-time optimisation** (`-flto`): inlining across files.
- **A faster allocator** when the hint is `alloc`:
  `LD_PRELOAD=libjemalloc.so.2` (or tcmalloc, mimalloc), with no rebuild.
  It helps most with several threads.
- **Threads of numeric libraries.** OpenMP and BLAS start a thread per core
  of the machine, whatever the container's CPU limit: set `OMP_NUM_THREADS`
  (`OPENBLAS_NUM_THREADS`, `MKL_NUM_THREADS`) to the CPUs available. The
  sign is `cpu-wait` with `blame.kind` `cgroup`.
- **Profile-guided optimisation** (`-fprofile-generate`, run, then
  `-fprofile-use`): often 10 to 20% on branchy code.
- **A faster linker** for a slow build, not a slow program: `-fuse-ld=mold`
  or `lld`.

## cpu

| In the stack (`code.hints` id) | It means | What usually works |
| --- | --- | --- |
| `malloc`, `free`, `operator new` (`alloc`) | Allocating per item | Reuse buffers; `reserve()`; pass `const T&`, `std::string_view`, `std::span`; `std::move`; `emplace_back`; an arena or pool for small objects of one lifetime |
| `memcpy`, `memmove`, `memset` (`copying`) | Large values copied, buffers cleared | `for (const auto& x : v)`, not `for (auto x : v)`; pass and return by reference or move; `reserve` so a vector is not copied as it grows; do not `memset` a whole buffer per iteration |
| `std::basic_string<…>::*` (`strings`) | Strings built and copied | `string_view` parameters; `reserve`; `+=` instead of `a + b + c` |
| `std::_Rb_tree*`, `std::_Hashtable*` (`node-containers`) | `std::map`, `std::set`, `std::unordered_map` | `reserve`; a flat hash map (Abseil, Boost, `ankerl::unordered_dense`); a sorted `std::vector` for small sets; `std::vector` instead of `std::list` |
| `std::basic_ostream*`, `std::locale` (`iostream`) | Formatted stream I/O | `'\n'` not `std::endl`; `sync_with_stdio(false)`; `std::to_chars`, `std::from_chars`, `std::format` instead of `stringstream` |
| `std::__detail::_Executor*` (`regex`) | `std::regex` | Build once; RE2, PCRE2 or CTRE on a hot path |
| `__cxa_throw`, `_Unwind_*` (`exceptions`) | Exceptions as control flow | Error codes, `std::optional`, `std::expected` on the hot path |
| `std::_Sp_counted_base*` (`shared-ptr`) | Reference counts | `const std::shared_ptr<T>&` or `T&` parameters; `unique_ptr`; `std::move` |
| `__dynamic_cast` (`dynamic-cast`) | RTTI on a hot path | A virtual function, a tag, `std::variant` |
| `strlen`, `strcmp`, `strcat` (`c-strings`) | C strings scanned again and again | Keep lengths; no `strlen` in a loop condition; a hash table instead of `strcmp` chains |
| `vfprintf`, `sscanf`, `getc` (`stdio`) | Formatting and character I/O | Bulk `fread` and `fwrite`; `strtol` instead of `sscanf`; `getc_unlocked`; no `fflush` per line |
| page faults, `mmap`, `munmap` (`page-faults`) | Memory taken from the system and returned, over and over | Reuse big buffers; an allocator that keeps them |
| your function, `frame.kind` `app` | The loop itself | Below |

In your own loops, once the algorithm is right:

- **Memory layout.** Data that is read together should lie together: arrays
  of values instead of pointers to nodes, the hot fields of a struct apart
  from the cold ones. A linked structure pays a cache miss per step.
- **Calls the compiler cannot see through.** A virtual call, a function
  pointer or a `std::function` per element blocks inlining and
  vectorisation: a template parameter or a lambda instead. A function in
  another file needs LTO to be inlined.
- **Invariants.** Hoist what does not change out of the loop; `restrict` or
  local copies when aliasing keeps the compiler from doing it.
- **False sharing.** Threads that write to neighbouring fields fight over
  one cache line: each thread's counter on its own line (`alignas(64)`).
  The sign is `cpu` that grows with threads while throughput does not.

## lock, cpu-wait

- A mutex (`where` is the waiter; `blame` names the holder for glibc
  mutexes): do less inside, split the lock, keep per-thread data and merge.
- More threads than CPUs give `cpu-wait`: size pools to the CPUs the process
  may use, which under a container limit is not the machine's count.
- `sched_yield` in a loop (hint `spinning`) is a spin lock: block instead.

## disk, peer

- `write()` per small record: buffer (`fwrite`, `setvbuf`, your own buffer).
  `fsync` per record: sync per batch; `fdatasync` when metadata does not
  matter.
- Small request and reply messages over TCP that stall about 40 ms: Nagle's
  algorithm with delayed ACKs. Set `TCP_NODELAY`, or send each message in
  one `write` or `writev`.
- A blocking `connect`, `getaddrinfo` or `read` on the thread that should be
  serving: wits shows that thread in `peer` and names the address.

## Measuring

- Pin the comparison: the same flags, input and machine. Use `-n 5`: the
  first run often differs from the rest, because nothing is cached yet.
- For one function, a benchmark harness (Google Benchmark) is more exact
  than timing the program, and `DoNotOptimize` keeps the work from being
  removed.
