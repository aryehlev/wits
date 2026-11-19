# The JVM (Java, Kotlin, Scala, Clojure): where the time usually goes

Read this when `code.lang` is `jvm`. Fixes are listed cheapest first.
`general.md` covers what holds in every language.

## See the program's functions

- Run `jcmd <pid> Compiler.perfmap` (JDK 17 and later) and ask wits again.
  It writes the names of the methods compiled so far: run it after warm-up,
  and again later if new code has become hot.
- Start the JVM with `-XX:+PreserveFramePointer` for whole stacks. Without
  changing the command line: `JAVA_TOOL_OPTIONS=-XX:+PreserveFramePointer`.
  Restarting a service is the user's decision.
- A compiled method includes the methods inlined into it, so a hot method
  may be hot because of a callee that has no frame of its own.
- Without names, wits still shows the JVM's own threads and functions (the
  collector, the JIT compilers, monitors), which the hints read. For the Java
  line, use the JVM's tools for that step: `jcmd <pid> Thread.print` (what
  each thread is doing now), JDK Flight Recorder (`jcmd <pid> JFR.start`),
  or async-profiler.

## Settings before code (ease 1)

- **Heap size.** The collector's cost climbs steeply when live data nears
  the limit. `-Xmx` with room above the live set, and `-Xms` equal to it for
  a server. `-Xlog:gc` shows how often it runs and how much it frees.
- **In a container** the default heap is a quarter of the memory limit:
  `-XX:MaxRAMPercentage=75`. With less than two CPUs or about 1.8 GB, the
  JVM picks the single-threaded Serial collector by itself: choose one
  (`-XX:+UseG1GC`, `-XX:+UseParallelGC`).
- **The collector.** `-XX:+UseParallelGC` gives the most throughput for
  batch work; G1 (the default) balances; ZGC (`-XX:+UseZGC`) keeps pauses
  short at some cost in throughput.
- **Start-up** (hint `warm-up`): a class-data archive
  (`-XX:+AutoCreateSharedArchive -XX:SharedArchiveFile=app.jsa`, JDK 19 and
  later), `-XX:TieredStopAtLevel=1` for short-lived tools, or a native image.
- **Thread and pool sizes.** A connection pool or executor smaller than the
  load makes requests queue (they show as `lock` or idle, waiting for the
  pool); far larger than the CPUs gives `cpu-wait`.

## cpu

| In the stack (`code.hints` id) | It means | What usually works |
| --- | --- | --- |
| `G1*`, `PS*`, `ZMark*`, `MemAllocator::*` (`gc`) | Allocation rate, or a heap that is too small | Fewer temporary objects: no boxed numbers in hot collections (`Map<Integer, Long>`: primitive collections such as fastutil, or arrays); no streams and lambdas in the innermost loop; `StringBuilder` instead of `+=` in a loop; presized `ArrayList` and `HashMap` |
| `C2Compiler`, `CompileBroker`, `Interpreter`, `ClassLoader` (`warm-up`) | Start-up: class loading, interpreting, JIT | Measure after warm-up, or shorten start-up with the settings above |
| `Throwable.fillInStackTrace` (`exceptions`) | Exceptions as control flow | Return a value; check first; `writableStackTrace=false` for an exception thrown often |
| `java.lang.reflect.*` (`reflection`) | Reflection per call | Cache `Method` and `Field`; `MethodHandle`; generated mappers |
| `java.util.regex.*` (`regex`) | `String.split`, `matches`, `replaceAll` compiling a pattern per call | A `static final Pattern` |
| `java.util.Formatter` (`formatting`) | `String.format` on a hot path | Concatenation or `StringBuilder`; `log.debug("x={}", x)` instead of building the string |
| your method, `frame.kind` `app` | The algorithm or the data structure | `ArrayList` or `ArrayDeque` instead of `LinkedList`; `HashMap` instead of scanning a list; `contains` on a `Set`, not a `List`; hoist work out of loops |
| a framework, `frame.kind` `lib` | It is asked too often | ORM: see `peer`. Serialisation: reuse `ObjectMapper` (creating one per call is very slow), use streaming for big documents. Logging: lower the level, async appender |

## lock

- **`synchronized`, `ReentrantLock`** (hint `monitor`): less work inside the
  lock; `ConcurrentHashMap`, `LongAdder`, atomics; `ReadWriteLock` or
  `StampedLock` when reads dominate; several locks instead of one.
- `jcmd <pid> Thread.print` names the thread that holds each monitor and
  the ones waiting.
- Writing to `System.out` is synchronized and slow: a log line per item to
  the console serialises every thread.
- Before JDK 24, a virtual thread that blocks inside `synchronized` pins its
  carrier thread; use `ReentrantLock` there.

## peer

- **N+1 queries through an ORM.** Hibernate loads lazy relations one row at
  a time: `JOIN FETCH`, an entity graph, or `@BatchSize`. Log the SQL
  (`hibernate.show_sql`) and count the statements per request.
- **Row by row writes.** `hibernate.jdbc.batch_size` (with
  `order_inserts`), JDBC `addBatch` in one transaction, and the driver's
  rewrite of batches (`reWriteBatchedInserts=true` for PostgreSQL,
  `rewriteBatchedStatements=true` for MySQL).
- **Connection pool.** Threads waiting in the pool (HikariCP `getConnection`)
  mean the pool is too small, transactions too long, or connections leaked.
- **HTTP clients.** Share one client (`HttpClient`, OkHttp, Apache) so that
  connections are reused; set timeouts.
- **Blocking in the wrong pool.** Blocking I/O on the common `ForkJoinPool`
  (`parallelStream`, `CompletableFuture.supplyAsync` without an executor) or
  on an event loop (Netty, WebFlux, Vert.x) stalls everything that shares
  it. Give blocking work its own executor, or virtual threads (JDK 21).

## Measuring

- **Warm-up.** A JVM interprets first and compiles what gets hot. A run of
  a few seconds measures mostly that. For a service, ask wits about the
  running process after it has served load; for a method, use JMH.
- **Build tools keep a daemon.** `./gradlew build` only talks to a Gradle
  daemon that an earlier build started and that is not a child of your
  command: `wits run` shows the client waiting (`peer`). Ask wits about the
  daemon's pid, or measure with `--no-daemon`.
- Kotlin, Scala and Clojure run on the same JVM, and all of the above
  holds. Clojure: reflection from missing type hints is a common hot spot
  (`(set! *warn-on-reflection* true)`).
