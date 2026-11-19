# .NET (C#, F#): where the time usually goes

Read this when `code.lang` is `dotnet`. Fixes are listed cheapest first.
`general.md` covers what holds in every language.

## First: is it a Release build?

`dotnet run` and `dotnet build` default to **Debug**, which turns the JIT's
optimisations off. wits says so when it sees unoptimised methods on the
stack (hint `debug-build`). Measure `dotnet run -c Release`, or the output
of `dotnet publish` (Release by default since .NET 8).

## See the program's functions

- Start the program with `DOTNET_PerfMapEnabled=1` (`wits run` sets it). On
  Linux wits then shows managed methods by name.
- Without it, and on macOS, wits shows the runtime's own functions (the
  collector, the JIT, exception handling), which the hints read. For the C#
  line, use the .NET tools for that step: `dotnet-stack report -p <pid>`
  (what each thread is doing now), `dotnet-trace`, and `dotnet-counters
  monitor -p <pid>` for rates of collections, exceptions and thread-pool
  queueing.

## Settings before code (ease 1)

- **Server GC** for a multi-core server: `DOTNET_gcServer=1` or
  `<ServerGarbageCollector>true</ServerGarbageCollector>`. ASP.NET Core
  turns it on by itself; a console program or worker uses Workstation GC
  unless told.
- **In a container**, the collector sizes itself from the memory limit and
  the CPU limit. With one CPU it uses Workstation GC whatever is asked.
  `DOTNET_GCHeapHardLimit` bounds the heap explicitly.
- **Start-up** (hint `startup`): `<PublishReadyToRun>true</PublishReadyToRun>`
  ships code already compiled; Native AOT (`<PublishAot>true</PublishAot>`)
  for command-line tools. Keep tiered compilation on.
- **Thread pool.** If requests stall while the CPU is idle, see the section
  on blocking below: raising the pool's minimum (`ThreadPool.SetMinThreads`,
  or `<ThreadPoolMinThreads>` in the project) is a stopgap, not the fix.

## cpu

| In the stack (`code.hints` id) | It means | What usually works |
| --- | --- | --- |
| `gc_heap::*`, `JIT_New*` (`gc`) | Allocation rate | `Span<T>`, `stackalloc`, `ArrayPool<T>.Shared`; `StringBuilder` or `string.Create` instead of `+=` in a loop; structs for small short-lived values; a presized `List<T>` and `Dictionary<K,V>`; no boxing (a struct passed as `object` or as an interface) |
| `System.Linq.*` (`linq`) | LINQ in the inner loop | A plain loop over the array or list; `ToList()` once if the query is read more than once |
| `IL_Throw`, `DispatchManagedException` (`exceptions`) | Exceptions as control flow | `TryParse`, `TryGetValue`, a result value |
| `System.Text.RegularExpressions.*` (`regex`) | A `Regex` built per call, or backtracking | `static readonly Regex`; `[GeneratedRegex]`; `RegexOptions.NonBacktracking` |
| `System.Reflection.*` (`reflection`) | Reflection per call | Cached delegates; source generators; one shared `JsonSerializerOptions` |
| `PreStubWorker`, `libclrjit` (`startup`) | JIT compiling at start | ReadyToRun or Native AOT; measure after warm-up |
| your method, `frame.kind` `app` | The algorithm or the collection | `Dictionary` or `HashSet` instead of `List.Contains` or `First(x => …)` in a loop; `foreach` over a `List<T>` or array, not over `IEnumerable<T>` built again each time; hoist work out of the loop |

## peer

- **Entity Framework Core.** A query per row from lazy loading: `Include`,
  or a projection with `Select`. `AsNoTracking()` for data that is only
  read. `AsSplitQuery()` when an `Include` of several collections multiplies
  rows. One `SaveChanges` for a batch, not one per entity.
- **`HttpClient`.** One shared instance or `IHttpClientFactory`. A new
  `HttpClient` per request opens a new connection each time and runs the
  machine out of sockets.
- **One call after another.** `await` in a loop over independent calls:
  `Task.WhenAll`, with a bound (`Parallel.ForEachAsync`).

## Blocking on async code: slow while idle

`.Result`, `.Wait()` and `.GetAwaiter().GetResult()` block a thread-pool
thread until a task finishes, and that task needs a pool thread to finish.
Under load the pool runs out: requests queue for seconds while the CPU is
idle. wits shows many threads idle or in `lock`, little `cpu`, and
`dotnet-counters` shows the thread-pool queue growing. The fix is `await`
all the way up. A higher minimum for the pool (`ThreadPool.SetMinThreads`)
hides it for a while.

## lock

- `lock` / `Monitor`: less work inside; `ConcurrentDictionary`,
  `Interlocked`, `ReaderWriterLockSlim` when reads dominate.
- A `lock` cannot hold across `await`; code that needs that uses
  `SemaphoreSlim`, whose waiters show as idle, not as `lock`.
- The console logger is synchronous and slow: a log line per item to the
  console serialises the threads. Lower the level or log to a file or a
  collector.

## disk, memory

- `FileStream` without a buffer, or `Flush(true)` per write: write through a
  `BufferedStream` or `StreamWriter` and flush per batch.
- Objects of 85,000 bytes or more go to the large object heap, collected
  only with the oldest generation: allocating big arrays per request makes
  those collections frequent. Rent them from `ArrayPool<T>`.

## Measuring

- **Warm-up.** Methods start in a quick, unoptimised tier and are recompiled
  when hot. A run of a few seconds measures mostly start-up. For a service,
  ask wits about the running process under load; for a method, use
  BenchmarkDotNet (it refuses Debug builds).
- Measure the built program (`dotnet bin/Release/net8.0/app.dll`, or the
  published executable), not `dotnet run`, which checks and may rebuild the
  project first.
