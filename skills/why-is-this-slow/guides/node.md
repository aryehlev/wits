# Node.js: where the time usually goes

Read this when `code.lang` is `node`: JavaScript or TypeScript on Node.js.
Deno shares the engine (V8) and most of this; Bun does not, and its flags
differ. Fixes are listed cheapest first. `general.md` covers what holds in
every language.

## See the program's functions

- Start node with `--perf-basic-prof` (`wits run` sets it through
  `NODE_OPTIONS`). On Linux wits then shows JavaScript functions with file and
  line.
- Functions still in the interpreter all show as one trampoline. Add
  `--interpreted-frames-native-stack` to see them by name.
- On macOS only native frames show: V8's own C++ (`v8::internal::…`), which
  the hints read.

## The one thing to know: one thread runs your code

All JavaScript runs on the main thread. While it computes, nothing else is
served: every other request, timer and reply waits. So for a server:

- `cpu` on the main thread is everyone's latency. Find the function, make it
  cheaper, or move it off the thread (`worker_threads`, a pool such as
  piscina, or another process).
- A blocking call in a request path stops the world: `fs.readFileSync`,
  `crypto.pbkdf2Sync`, `zlib.gzipSync`, `child_process.execSync`,
  `JSON.parse` of a large body. Use the async forms.
- To use more than one core, run several processes (`cluster`, a process
  manager, more containers).

## Settings before code (ease 1)

- `NODE_ENV=production`. Frameworks and libraries (Express, React and many
  more) do extra checks and skip caches without it.
- `--max-semi-space-size=64` (MB, default 16) when the hint is `gc`: a larger
  young generation, fewer scavenges.
- `--max-old-space-size=<MB>` when the heap runs near its limit and the
  collector runs back to back. In a container, set it below the memory limit.
- `UV_THREADPOOL_SIZE` (default 4). File system calls, `dns.lookup`, `crypto`
  and `zlib` share four threads. If they queue (requests slow while CPU is
  free, many threads idle), raise it.
- `NODE_COMPILE_CACHE=<dir>` (Node 22 and later) keeps compiled code between
  runs of a command-line tool.

## cpu

| In the stack (`code.hints` id) | It means | What usually works |
| --- | --- | --- |
| `Heap::CollectGarbage`, `Scavenger`, `MarkCompactCollector` (`gc`) | Allocation rate: objects, arrays, closures made per item | No copies in loops: `{...acc, [k]: v}` or `acc.concat(x)` inside `reduce` is quadratic; one loop instead of `map().filter().map()` on big arrays; reuse buffers; streams instead of whole files |
| `Compiler::`, `Parser`, `node:internal/modules` (`startup`) | Loading and compiling modules | Load heavy modules where used; bundle; compile cache; run compiled JavaScript, not `ts-node` |
| `*IC_Miss`, `*IC_Megamorphic`, `Deoptimizer` (`slow-properties`) | Objects of many shapes at one site, or code being deoptimised | Same properties in the same order from one constructor; no `delete`; arrays of one element kind; `Map` for dynamic keys |
| `RegExp*`, `Irregexp` (`regex`) | Backtracking, or a RegExp built per call | Simpler anchored patterns; hoist the RegExp; string methods for fixed text |
| `JsonParser`, `JsonStringifier` (`json`) | Large documents on the main thread | Smaller payloads; parse once; no JSON round trip to copy; stream |
| your function, `frame.kind` `app` | The algorithm | `Set` or `Map` instead of `includes` or `find` in a loop; no `shift()` in a loop on a big array; `Buffer.concat` once after the loop, not in it; cache repeated work |
| a package, `frame.kind` `lib` | It is called per item | A bulk call; a lighter library for the hot path (a validator or date library can dominate a handler) |

Creating an `Error` captures a stack trace: exceptions as ordinary control
flow on a hot path are slow.

## peer

- **`await` in a loop.** Each iteration waits for the one before. For
  independent calls use `Promise.all` over the batch, with a limit (p-limit)
  when the batch is large.
- **N+1 queries.** A query per item, often hidden in an ORM's lazy relation
  or a GraphQL resolver. Fetch with a join or an `IN` list; batch resolvers
  with DataLoader.
- **Connections.** `fetch` and the global agents of Node 19 and later reuse
  connections. Older code with its own `http.Agent` needs `keepAlive: true`.
  Give the database client a pool, and size it: requests waiting for a free
  connection look like a slow database.
- **Streams.** Reading a whole upload or file into memory before sending it
  on: pipe it (`stream.pipeline`).

## disk, memory, lock

- `fs.*Sync` in a server is covered above. In a build script it is fine.
- Many small `fs.writeFile` or `appendFile` calls: one write stream.
- Memory that grows: a cache or a `Map` with no bound, listeners that are
  never removed, closures holding large objects. `--heapsnapshot-near-heap-limit=1`
  writes a snapshot before the heap limit is hit.
- JavaScript has no locks to contend on. `lock` in a Node process is a file
  lock (a package manager, a build tool) or a native library.

## Measuring

- The first calls of a function run in the interpreter and get compiled as
  they heat up. A short run measures start-up and warm-up: for a server,
  watch the running process under load instead.
- `npm`, `yarn` and `pnpm` scripts start a shell and another node: `wits run`
  shows each process. Measure the command itself when the wrapper is not the
  question.
- TypeScript run through `ts-node` type-checks at start: seconds of `cpu`
  before the program begins. `tsc` ahead of time, or a transpiler that only
  strips types.
