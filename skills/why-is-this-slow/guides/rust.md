# Rust: where the time usually goes

Read this when `code.lang` is `rust`. Fixes are listed cheapest first.
`general.md` covers what holds in every language.

## First: is it a release build?

A dev build (`cargo build`, `cargo run`, `cargo test` without `--release`)
is often 10 to 50 times slower, and its hot spots are not those of the
optimised program. wits says so when it sees the standard library's debug
checks on the stack (hint `debug-build`). Measure
`./target/release/<name>`, built with `cargo build --release`. Nothing else
in this guide matters until then.

## See the program's functions

- Whole stacks need frame pointers:
  `RUSTFLAGS="-C force-frame-pointers=yes" cargo build --release`. The
  standard library has had them since Rust 1.79. wits says so when stacks
  stop early.
- File and line need debug info, and it costs no speed:

  ```toml
  [profile.release]
  debug = "line-tables-only"
  ```

- Do not strip the binary you measure (`strip = true`, `-C strip`).
- Run the binary itself under `wits run`, not `cargo run`: cargo adds its own
  start-up, and may rebuild.

## Settings before code (ease 1)

```toml
[profile.release]
lto = "fat"          # or "thin": inlining across crates
codegen-units = 1    # better optimisation, slower build
panic = "abort"      # smaller, slightly faster code, if nothing catches panics
```

- **`-C target-cpu=native`** (in `RUSTFLAGS`) when the binary runs where it
  is built: newer instructions and wider vectors for numeric loops.
- **A faster allocator** when the hint is `alloc`: mimalloc or jemalloc
  (`tikv-jemallocator`) as `#[global_allocator]`, two lines. It matters most
  for programs with several threads, and for static musl builds, whose
  allocator is slow.
- **Profile-guided optimisation** (`cargo-pgo`) for another 10% or so on
  branchy code.

## cpu

| In the stack (`code.hints` id) | It means | What usually works |
| --- | --- | --- |
| `malloc`, `__rust_alloc`, `alloc::raw_vec::*` (`alloc`) | Allocating per item | Reuse a `Vec` or `String` across iterations (`clear()` keeps the capacity); `with_capacity`; borrow instead of `clone()`, `to_string()`, `to_vec()`; iterate instead of `collect()` into a temporary; `SmallVec` for short vectors |
| `<… as Clone>::clone` (`cloning`) | Copies of owned data | `&str`, `&[T]`, `&T`; move at the last use; `Arc` for shared read-only data; `Cow` |
| `memcpy`, `memmove` (`copying`) | Large values moved or copied | Pass big structs by reference; `Box` a large enum variant; do not rebuild a big value per iteration |
| `core::hash::sip::*`, `DefaultHasher` (`siphash`) | The default hasher | `FxHashMap` (rustc-hash), `ahash` or `foldhash` where keys are not hostile; the `entry` API for one lookup instead of two |
| `core::fmt::*` (`formatting`) | `format!`, `to_string()`, `write!` | Out of the hot loop; `write!` into a reused `String`; `itoa`, `ryu`; no formatting for log levels that are off |
| `regex::…::Regex::new`, `regex_syntax::*` (`regex-compile`) | A regex compiled per call | `static RE: LazyLock<Regex>`; `str` methods or `memchr` for fixed text |
| `std::io::stdio::*` (`stdout`) | `println!` locking and flushing per line | `BufWriter::new(stdout().lock())` and `writeln!` |
| `read`, `write` (`read-write`) | `File`, `TcpStream` and `Stdin` are not buffered | `BufReader` and `BufWriter`; read a file whole with `fs::read`; `read_line` into a reused `String` instead of `lines()` |
| your function, bounds checks or a tight loop | The loop itself | Iterators (`iter`, `zip`, `chunks_exact`, `windows`) instead of indexing, so checks vanish and loops vectorise; hoist invariants; `rayon`'s `par_iter` when the items are independent and the work per item is not tiny |
| a `dyn Trait` call per item | Dynamic dispatch blocking inlining | Generics or an enum on the hot path |

serde_json: `from_slice` or `from_str` on data already in memory is much
faster than `from_reader`; borrow strings (`&'a str`, `#[serde(borrow)]`)
instead of allocating each one.

## lock

- **`Mutex` or `RwLock`** (hint `mutex`): hold it for less, shard it
  (`DashMap`, per-thread state merged at the end), use atomics for counters
  and flags.
- **`Arc` cloned in a hot loop** by several threads: the reference count is
  one contended cache line. Clone once per task, pass `&T` inside.
- A `Mutex` guard held across an `.await` blocks every task that wants the
  lock, and with a std `Mutex` blocks the worker thread too.

## Async (Tokio and others)

- wits watches threads. A task awaiting I/O holds no thread, so its wait
  shows as an idle worker, or as `peer` on a connection, not as a stack in
  your handler. `tokio-console` shows tasks.
- **Blocking the runtime.** CPU-heavy work, `std::fs`, `std::thread::sleep`,
  a blocking client or a contended std `Mutex` inside a task holds a worker
  thread and stalls the tasks behind it: wits shows worker threads in `cpu`,
  `disk` or `lock` while requests wait. Move it to
  `tokio::task::spawn_blocking`, or to rayon for CPU work.
- `tokio::fs` hops to a blocking thread for every call: for many small file
  operations, do them together in one `spawn_blocking`.
- Independent futures awaited one after another run in series: `join!`,
  `try_join!`, or a `JoinSet` with a bound.

## peer, disk

- A query per row: one query, or a batch; `sqlx` and `diesel` pools need a
  size that fits the concurrency, and waits for the pool look like a slow
  database.
- `reqwest`: build one `Client` and share it. A new client per request means
  a new connection and handshake each time.
- `File::sync_all` or `sync_data` per record: sync per batch.

## Measuring

- `cargo bench` with Criterion measures a function more exactly than timing
  the program, and guards against the optimiser removing the work
  (`std::hint::black_box`).
- Compare like with like: the same profile, features and `RUSTFLAGS` before
  and after. `cargo build --release` first, then
  `wits run -n 5 -- ./target/release/<name> <args>`.
- `cargo test` builds in the dev profile: a slow test suite may only be an
  unoptimised one. Try `cargo test --release`, or `opt-level = 1` for the
  `dev` profile (and `opt-level = 3` for dependencies:
  `[profile.dev.package."*"]`).
