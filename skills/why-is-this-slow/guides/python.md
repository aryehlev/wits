# Python: where the time usually goes

Read this when `code.lang` is `python`. It covers CPython. Fixes are listed
cheapest first. `general.md` covers what holds in every language (databases,
networks, disks, containers).

## See the program's functions

- Python 3.12 and later on Linux publish their functions when started with
  `PYTHONPERFSUPPORT=1` (or `python -X perf`). `wits run` sets it. A service
  that is already running has to be restarted with it: that is the user's
  decision.
- Without it, and always on macOS and before 3.12, wits shows the
  interpreter's C functions (`_PyEval_EvalFrameDefault`, `PyObject_GetAttr`)
  instead of yours. The hints still work: they read those C functions. To
  find the Python line, use a Python profiler for that step:
  `py-spy dump --pid <pid>` (what it is doing now) or `py-spy top`.
- Time inside a C extension shows the extension's own functions, with
  `frame.kind` `lib` and the package's name.

## Settings before code (ease 1)

- **A newer Python.** 3.11 runs pure-Python code 10 to 60% faster than 3.10,
  and each release since adds some.
- **Threads of numeric libraries.** NumPy, SciPy and PyTorch start a thread
  per core in each process. With several worker processes, or under a
  container CPU limit, that is far more threads than CPUs, and wits shows
  `cpu-wait`: set `OMP_NUM_THREADS` (and `OPENBLAS_NUM_THREADS`,
  `MKL_NUM_THREADS`) to the CPUs each process may use.
- **Tools left attached.** Coverage (`--cov`), a debugger and `cProfile`
  slow everything (hint `tracing`). Measure without them;
  `COVERAGE_CORE=sysmon` on 3.12 and later makes coverage much cheaper.
- **Workers.** A sync web worker handles one request at a time: with every
  worker in `peer`, add workers or threads (gunicorn `--workers`,
  `--threads`), or fix what they wait for.

## cpu

| In the stack (`code.hints` id) | It means | What usually works |
| --- | --- | --- |
| `_PyEval_EvalFrameDefault` and the call and attribute machinery on top (`interpreter`) | Python-level code is the cost: a loop running in the interpreter | Let C do the loop: built-ins (`sum`, `min`, `sorted`, `any`, `str.join`), comprehensions, `itertools`, `collections.Counter` and `defaultdict`. A `dict` or `set` instead of `x in some_list`. `functools.lru_cache` for repeated calls. NumPy or pandas operations on whole columns instead of a Python loop over rows (`iterrows`, `apply(axis=1)`) |
| `gc_collect_*`, `deduce_unreachable` (`gc`) | The cyclic collector, triggered by creating many container objects | `gc.disable()` around a bulk load, `gc.freeze()` after start-up in a worker, or higher `gc.set_threshold` |
| `_PyObject_Malloc`, `_Py_Dealloc` (`objects`) | Many short-lived objects | Generators instead of lists, `str.join` instead of `+=`, `__slots__`, arrays instead of lists of numbers |
| `PyImport_*`, `<frozen importlib>` (`imports`) | Start-up: importing modules and running their top level | `python -X importtime` names the slow imports. Import heavy packages inside the function that uses them. Do no work at import time |
| `sre_*`, `re/_parser.py` (`regex`) | Matching, or compiling patterns again and again | `re.compile` once at module level; simpler, anchored patterns; `str` methods for fixed text |
| `scanner_call`, `encoder_*` (`json`) | The `json` module | Parse once. `orjson` or `msgspec` for a hot path |
| `_pickle_*` (`pickle`) | Objects copied between processes | Bigger `chunksize`, pass big inputs once (initializer, shared memory, a path), return less |
| `CTracer_*`, `trace_trampoline` (`tracing`) | Coverage, a debugger or a profiler | Measure without it |
| a package's functions, `frame.kind` `lib` | The library is doing what it was asked | Ask for less: select the columns and rows needed, vectorised calls instead of one call per element, a bulk API |

When the loop itself must stay in Python and is still too slow: process the
data in parallel processes (below), or compile the hot module (Cython, mypyc)
or run it on PyPy. These are ease 0.1 to 0.4; exhaust the table first.

## lock, and threads that do not help

- **The GIL** (hint `gil`). One thread runs Python code at a time. Threads
  help only while they wait for I/O, or inside C code that releases the GIL
  (NumPy, `hashlib`, `zlib`, most database drivers). For CPU work use
  processes: `concurrent.futures.ProcessPoolExecutor` or `multiprocessing`.
  The free-threaded build (`python3.13t` and later) has no GIL.
- Without Python's frames, wits may count `Event.wait`, `Queue.get` and
  `Thread.join` as `lock`: they wait through a lock. With
  `PYTHONPERFSUPPORT=1` it counts them as idle. A thread pool whose workers
  all show `lock` in `Queue.get` has no work, not a contended lock.
- Processes cost start-up and pickling. Starting a pool per call, or sending
  a large object with every task, can cost more than the work.

## peer

- **N+1 queries from an ORM.** A query per object for its relations. Django:
  `select_related` (joins) and `prefetch_related`; SQLAlchemy: `joinedload`,
  `selectinload`. Fetch only the columns used (`values`, `only`).
- **Row by row.** `executemany`, `bulk_create`, `COPY`, or one multi-row
  statement instead of an `INSERT` per row, all in one transaction.
- **A new connection per request.** `requests.get(...)` in a loop opens a
  connection each time: use one `requests.Session` (or one `httpx.Client`).
  Give the database driver a pool.
- **One request after another.** Run independent calls together: a
  `ThreadPoolExecutor` for blocking clients, `asyncio.gather` with a
  semaphore for async ones.
- **asyncio with a blocked loop.** One slow callback stalls every task:
  `time.sleep`, `requests`, a blocking driver, or plain CPU work inside a
  coroutine. wits shows the loop thread in `cpu`, or in `peer` on one socket,
  while everything else waits. Use the async client, or
  `asyncio.to_thread`. `PYTHONASYNCIODEBUG=1` logs callbacks over 100 ms.

## disk, memory

- Many small `write` calls, or `flush` and `os.fsync` after each: write
  through the default buffer and sync once per batch.
- SQLite: one transaction around many inserts; `PRAGMA journal_mode=WAL`.
- pandas: read only the needed columns (`usecols`), give `dtype`s (`category`
  for repeated strings), read in chunks (`chunksize`), prefer Parquet to CSV.
- Whole files or result sets held in memory: iterate (a file object, a
  server-side cursor, a generator) instead of `read()` or `fetchall()`.

## Measuring

- Starting the interpreter and importing takes from 20 ms to several
  seconds: for a short command, that is the time. `python -X importtime`
  shows where it goes.
- `pytest --durations=10` names the slow tests and the slow fixtures. A
  fixture that sets up a database or a server for every test usually wants a
  wider scope.
