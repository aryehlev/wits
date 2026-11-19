# Go: where the time usually goes

Read this when `code.lang` is `go`. Fixes are listed cheapest first.
`general.md` covers what holds in every language.

## See the program's functions

- Nothing to set: Go binaries keep frame pointers and symbols, and wits shows
  function, file and line, inlined functions included.
- Do not strip the binary you measure (`-ldflags="-s -w"`): wits takes the
  names from its symbols.
- wits watches threads. A goroutine waiting for the network, a channel or a
  timer is parked and holds no thread, so its wait is not a stack in your
  handler: the process may show as idle, or as `peer` on a connection. To see
  what goroutines wait for, use Go's own tools:
  `/debug/pprof/goroutine?debug=2`, the block and mutex profiles, and
  `go tool trace`.

## Settings before code (ease 1)

- **`GOGC`** (default 100: collect when the heap has doubled). `GOGC=200` or
  more trades memory for fewer collections. With a small live heap the
  collector runs very often, and this alone can remove most of the `alloc-gc`
  time.
- **`GOMEMLIMIT`** (Go 1.19 and later) bounds the heap. Set it under the
  container's memory limit, with a high `GOGC`, to use the memory you have.
- **`GOMAXPROCS` in a container.** Before Go 1.25 it is the number of cores
  of the host, whatever the CPU limit: too many running threads, throttling
  (`cpu-wait` with `blame.kind` `cgroup`) and wasted GC workers. Set it to
  the limit, or import `go.uber.org/automaxprocs`. Go 1.25 reads the limit
  itself.
- **Profile-guided optimisation** (Go 1.21 and later): a CPU profile saved
  as `default.pgo` in the main package gives a few percent, up to about 14%.
- **Not `-race`.** The race detector slows a program 2 to 20 times (hint
  `race-detector`).

## cpu

| In the stack (`code.hints` id) | It means | What usually works |
| --- | --- | --- |
| `runtime.mallocgc`, `runtime.gcBgMarkWorker`, `runtime.scanobject` (`alloc-gc`) | Allocation rate, and the collector that follows it | `make([]T, 0, n)` and `make(map[K]V, n)`; reuse buffers (`sync.Pool`, `buf.Reset()`); `strings.Builder`; no `[]byte(s)` and `string(b)` back and forth; values instead of pointers in slices and maps (fewer objects to trace). `go build -gcflags=-m` shows what escapes; `go test -bench . -benchmem` counts allocations |
| `runtime.mapaccess*`, `runtime.mapassign*` (`maps`) | Hashing and lookups | Size the map; one lookup instead of two; integer or struct keys; a slice for small dense keys |
| `encoding/json.*` (`json`) | Reflection-driven JSON | Typed structs, not `map[string]any`; a generated or faster codec |
| `reflect.*` (`reflection`) | A library inspecting types per call | Call it per batch, cache, or generate code |
| `regexp.*` (`regex`) | The regexp engine, or `MustCompile` inside a function | Compile at package level; `strings` functions for fixed text |
| `fmt.*` (`fmt`) | `Sprintf` and friends on a hot path | `strconv`, concatenation, `strings.Builder`; no formatting for log lines that are off |
| `runtime.schedule`, `runtime.findRunnable`, `runtime.chansend` (`scheduler`) | Goroutines exchanging tiny pieces of work | Batch what is sent over channels; buffered channels; a bounded worker pool; a mutex for simple shared state |
| `runtime.cgocall` (`cgo`) | Calls into C | Fewer, bigger calls across the boundary |
| `runtime.memmove`, `runtime.duffcopy` | Copying large values | `for i := range xs` and `xs[i]` instead of `for _, x := range xs` when the elements are big structs; pass pointers to large structs |
| `syscall.*`, `internal/poll.*` (`read-write`) | Small reads and writes | `bufio.Reader` and `bufio.Writer` (and `Flush`); `io.Copy` |

## lock

- **`sync.Mutex`** (hint `mutex`): shorten the critical section, shard the
  data, use atomics for counters. `RWMutex` helps only when reads dominate
  and take long enough to matter.
- `runtime.SetMutexProfileFraction(5)` with the pprof mutex profile names
  the contended lock and who holds it.
- A goroutine blocked on a channel is not `lock` to wits: see the note on
  goroutines above.

## peer

- **`database/sql`.** A query per row (N+1): one query with a join or
  `IN`. Rows inserted one by one: a multi-row insert or `COPY`, in one
  transaction. Requests waiting in `db.conn` want a larger pool
  (`SetMaxOpenConns`, `SetMaxIdleConns`) or shorter transactions; forgetting
  `rows.Close()` leaks connections until the pool is empty.
- **`net/http` client.** Share one `http.Client`. Read the response body to
  the end and close it, or the connection is not reused. The default
  transport keeps 2 idle connections per host: raise `MaxIdleConnsPerHost`
  for a busy client. Set a timeout: the default client has none.
- **One call after another.** Run independent calls in goroutines with
  `errgroup`, with `SetLimit`.

## disk, memory

- `os.File` is not buffered: each `Write` is a system call. Wrap it in
  `bufio.Writer`.
- `f.Sync()` per record: sync per batch.
- `memory` with `blame.kind` `cgroup`: the heap grows to twice the live data
  before a collection by default. Set `GOMEMLIMIT` below the limit.

## Measuring

- **`go test` caches results.** A second run prints `(cached)` and takes no
  time: use `go test -count=1` when you measure it with `wits run -n 5`.
  `go build` caches too: measure the built binary, not `go run`.
- For a function, a benchmark is more exact than timing the program:
  `go test -bench X -count 10` before and after, compared with `benchstat`.
- Build the binary first (`go build -o app .`) and run that under
  `wits run`, so compile time is not in the measurement.
