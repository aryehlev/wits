//! Costs wits knows by name. A program's time is often spread over many
//! callers of the same few functions: the allocator and the garbage
//! collector, the regex engine, the interpreter's own loop, a lock. No
//! single hot spot shows that; the sum over every stack does. Each rule
//! here names such a cost by the functions that show it, and says what it
//! means and what usually fixes it. The guides (guide.rs) say more.
//!
//! A hint is a measurement, not advice from thin air: it carries the share
//! of the cause's time that passed through those functions, and the one
//! seen most.

use crate::answer::Cat;
use crate::code::{Frame, Kind, Stacks};
use crate::lang::{self, Lang};
use std::collections::HashMap;

/// Which stacks a rule may match.
#[derive(Clone, Copy, PartialEq)]
enum In {
    /// Any: the names are particular enough.
    Any,
    /// The matching function belongs to the rule's own language: its
    /// runtime's binary, or a name only that language has.
    Own,
    /// By the language of the code the time is in (the first frame from
    /// the leaf that tells): Rust.
    Rust,
    /// C, C++, or native code wits cannot tell the language of.
    C,
    /// Rust, C, C++ or unknown: not inside a managed runtime.
    Native,
}

/// Some functions call back out: a reflective call runs the method it was
/// given, a class initialiser and a tracer run the program. That time is
/// not theirs.
#[derive(Clone, Copy, PartialEq)]
enum Below {
    /// Everything under the function counts.
    All,
    /// Not once the stack is back in the program's own code.
    NotYours,
    /// Only the runtime's own code: not the program's, not a library's.
    Runtime,
}

struct Rule {
    /// The language whose guide says more. `None`: that of the stacks.
    lang: Option<Lang>,
    within: In,
    id: &'static str,
    cats: &'static [Cat],
    /// Functions anywhere in the stack: the time is theirs and their callees'.
    under: &'static [&'static [&'static str]],
    /// Functions on top of the stack: the time is in the function itself.
    top: &'static [&'static str],
    /// What may run below an `under` function for the time to count.
    below: Below,
    /// Functions that make the time another rule's: printf under an
    /// ostream is the ostream's.
    unless: &'static [&'static str],
    /// The share of the cause's time from which it is worth saying.
    min: f64,
    /// A sign about the whole program (a debug build), not a share of it.
    whole: bool,
    means: &'static str,
    fix: &'static str,
}

const R: Rule = Rule {
    lang: None,
    within: In::Any,
    id: "",
    cats: &[Cat::Cpu],
    under: &[],
    top: &[],
    below: Below::All,
    unless: &[],
    min: 0.05,
    whole: false,
    means: "",
    fix: "",
};

const fn own(lang: Lang) -> Rule {
    Rule {
        lang: Some(lang),
        within: In::Own,
        ..R
    }
}
const GO: Rule = own(Lang::Go);
const PY: Rule = own(Lang::Python);
const JS: Rule = own(Lang::Node);
const JVM: Rule = own(Lang::Jvm);
const NET: Rule = own(Lang::Dotnet);
const RB: Rule = own(Lang::Ruby);
const BEAM: Rule = own(Lang::Beam);
const CXX: Rule = own(Lang::C);
const RUST: Rule = Rule {
    lang: Some(Lang::Rust),
    within: In::Rust,
    ..R
};

// A pattern is part of the function's name, unless it starts with:
//   =  the whole name          ^  the start of the name
//   @  the start of the module #  part of the source file's path
//   %  part of the name as the runtime wrote it (with parameters and tier)
//   ~  the start of a Rust path, or of the type the function is a method
//      of: "~core::fmt::" is "core::fmt::write" and
//      "<core::fmt::Formatter as core::fmt::Write>::write_str", but not a
//      function that only has such a type among its parameters
// Names are matched without C++ return types and parameters (lang::bare).

const MALLOC: &[&str] = &[
    "=malloc", "=free", "=calloc", "=realloc", "=cfree", "=posix_memalign", "=aligned_alloc", "=memalign",
    "^__libc_malloc", "^__libc_free", "^__libc_calloc", "^__libc_realloc", "^__GI___libc_malloc",
    "^__GI___libc_free", "^__GI___libc_realloc", "^_int_malloc", "^_int_free", "^_int_realloc",
    "^_int_memalign", "=malloc_consolidate", "=sysmalloc", "=systrim", "^tcache_", "^operator new",
    "^operator delete", "@libsystem_malloc", "@libjemalloc", "@libtcmalloc", "@libmimalloc", "^je_malloc",
    "^je_free", "^je_realloc", "^_rjem_", "^mi_malloc", "^mi_free", "^mi_realloc", "^mi_heap_", "^tc_malloc",
    "^tc_free", "^tc_new", "^tc_delete",
];
const RUST_ALLOC: &[&str] = &[
    "__rust_alloc", "__rust_dealloc", "__rust_realloc", "__rdl_alloc", "__rdl_dealloc", "__rdl_realloc",
    "~alloc::alloc::", "~alloc::raw_vec::",
];

static RULES: &[Rule] = &[
    // ------------------------------------------------ signs about the build
    Rule {
        id: "debug-build",
        under: &[&["::precondition_check"]],
        min: 0.001,
        whole: true,
        means: "the standard library's debug checks are on the stack: this looks like a dev build, without optimisations",
        fix: "Measure and ship a release build: cargo build --release (cargo run --release, cargo test --release). A dev build is often 10 to 50 times slower, and what is hot in it says little about the optimised program. To keep file and line in a release build, set debug = \"line-tables-only\" under [profile.release].",
        ..RUST
    },
    Rule {
        id: "debug-build",
        under: &[&["%[MinOptJitted]"]],
        min: 0.2,
        whole: true,
        means: "methods compiled without optimisation ([MinOptJitted]) are on the stack: this looks like a Debug build",
        fix: "Measure a Release build: dotnet run -c Release or dotnet build -c Release (they default to Debug, which turns the JIT's optimisations off; dotnet publish defaults to Release since .NET 8).",
        ..NET
    },
    Rule {
        id: "race-detector",
        under: &[&["^runtime.racefuncenter", "^runtime.racefuncexit", "^runtime.raceread", "^runtime.racewrite", "^runtime.racecall"]],
        min: 0.001,
        whole: true,
        means: "the race detector's hooks are on the stack: this binary was built with -race",
        fix: "The race detector slows a program 2 to 20 times and uses 5 to 10 times the memory. Measure a build without -race, and keep -race for a separate test run.",
        ..GO
    },
    Rule {
        id: "debug-containers",
        under: &[&["^__gnu_debug::", "^std::__debug::"]],
        min: 0.001,
        whole: true,
        means: "libstdc++'s debug mode is on the stack (_GLIBCXX_DEBUG): this is a checked build",
        fix: "Debug-mode containers check every access and every iterator. Measure an optimised build without it: -O2 or -O3 and no -D_GLIBCXX_DEBUG (with CMake, -DCMAKE_BUILD_TYPE=Release).",
        ..CXX
    },
    Rule {
        id: "sanitizer",
        under: &[&[
            "^__asan_", "^__tsan_", "^__msan_", "^__ubsan_", "^__lsan_", "^__hwasan_", "^__sanitizer", "^__interceptor_",
            "^__asan::", "^__tsan::", "^__msan::", "@libclang_rt.", "@libasan", "@libtsan", "@libubsan",
        ]],
        below: Below::NotYours,
        min: 0.001,
        whole: true,
        means: "a sanitizer's hooks are on the stack: the program is built with ASan, TSan, MSan or UBSan",
        fix: "A sanitizer slows a program 2 to 15 times and changes where its time goes. Measure a build without -fsanitize, and keep the sanitizer for a separate test run.",
        ..R
    },
    // ------------------------------------------------------- any language
    Rule {
        id: "page-faults",
        under: &[&[
            "=handle_mm_fault", "=__handle_mm_fault", "=do_user_addr_fault", "=exc_page_fault", "=asm_exc_page_fault",
            "=do_page_fault", "=do_mem_abort", "=do_translation_fault", "^clear_page", "^clear_huge_page",
            "^folio_zero_user", "=do_anonymous_page", "=__x64_sys_mmap", "=__x64_sys_munmap", "=__x64_sys_brk",
            "=__x64_sys_madvise", "=__arm64_sys_mmap", "=__arm64_sys_munmap", "=__arm64_sys_brk",
            "=__arm64_sys_madvise", "=__vm_munmap", "=vm_mmap_pgoff", "=mmap", "=mmap64", "=__mmap", "=munmap",
            "=__munmap", "=madvise", "=__madvise", "=brk", "=sbrk", "=__brk",
        ]],
        means: "the kernel mapping memory in and out: page faults on first touch, mmap, munmap and brk",
        fix: "The program keeps getting memory from the system and giving it back. Reuse large buffers instead of allocating one per iteration or request. In C and C++, glibc returns big blocks at once (M_MMAP_THRESHOLD, M_TRIM_THRESHOLD); jemalloc and tcmalloc keep them. Reading a memory-mapped file shows here too, and in a short run part of this is start-up that cannot be removed.",
        ..R
    },
    Rule {
        id: "read-write",
        under: &[&[
            "=read", "=write", "=__read", "=__write", "=__libc_read", "=__libc_write", "=__GI___libc_read",
            "=__GI___libc_write", "=__read_nocancel", "=__write_nocancel", "=pread", "=pwrite", "=pread64",
            "=pwrite64", "=readv", "=writev", "=__GI___writev", "=send", "=sendto", "=sendmsg", "=recv", "=recvfrom",
            "=recvmsg", "=__libc_send", "=__libc_recv", "=__send", "=__recv", "^__x64_sys_read", "^__x64_sys_write",
            "^__x64_sys_pread", "^__x64_sys_pwrite", "^__x64_sys_send", "^__x64_sys_recv", "^__arm64_sys_read",
            "^__arm64_sys_write", "^__arm64_sys_pread", "^__arm64_sys_pwrite", "^__arm64_sys_send",
            "^__arm64_sys_recv", "=ksys_read", "=ksys_write", "=vfs_read", "=vfs_write",
            "^internal/poll.(*FD).Read", "^internal/poll.(*FD).Write", "^internal/poll.(*FD).Pread",
            "^internal/poll.(*FD).Pwrite",
        ]],
        means: "read, write, send and recv system calls: CPU spent crossing into the kernel and copying the data",
        fix: "If the calls are small and many, that is the cost: read and write through a buffer (BufReader and BufWriter, bufio, setvbuf, io.BufferedWriter), send a message in one call instead of piece by piece, and batch. If they already move large blocks, this is the price of the I/O itself.",
        ..R
    },
    Rule {
        id: "clock",
        under: &[&[
            "=clock_gettime", "=__clock_gettime", "=__GI___clock_gettime", "=__vdso_clock_gettime",
            "=__kernel_clock_gettime", "=gettimeofday", "=__gettimeofday", "=__vdso_gettimeofday",
            "=__kernel_gettimeofday", "=__vdso_time", "^__x64_sys_clock_gettime", "^__arm64_sys_clock_gettime",
            "^__x64_sys_gettimeofday", "=mach_absolute_time", "=mach_continuous_time", "=clock_gettime_nsec_np",
            "=time.Now", "=time.now", "=time.Since",
        ]],
        means: "reading the clock",
        fix: "Something takes a timestamp per item: logging, metrics, tracing spans, timeouts or rate limits. Take one per batch, or sample. If the stack shows a clock_gettime system call inside the kernel, the machine's clock source is slow (a virtual machine without tsc or kvm-clock: see /sys/devices/system/clocksource/clocksource0/current_clocksource) and every read costs a system call.",
        ..R
    },
    Rule {
        id: "spinning",
        under: &[&[
            "=sched_yield", "=__sched_yield", "=__GI___sched_yield", "^__x64_sys_sched_yield",
            "^__arm64_sys_sched_yield", "=do_sched_yield", "=swtch_pri", "=thread_switch",
            "=std::thread::yield_now", "=std::this_thread::yield",
        ]],
        means: "spinning: a thread that gives the CPU up and asks for it again, waiting for something",
        fix: "A spin lock or a busy-wait loop is burning CPU while it waits. Block instead (a mutex, a condition variable, a channel), or spin a few times and then block. With more threads than CPUs, spinning makes the wait longer, not shorter.",
        ..R
    },
    Rule {
        id: "exceptions",
        under: &[&[
            "^__cxa_throw", "^__cxa_rethrow", "^__cxa_allocate_exception", "^__cxa_begin_catch", "^__cxa_end_catch",
            "^_Unwind_RaiseException", "^_Unwind_Resume", "^_Unwind_Find_FDE", "^__gxx_personality",
            "^uw_frame_state_for", "^uw_update_context", "^execute_cfa_program",
        ]],
        means: "throwing and catching C++ exceptions: unwinding the stack",
        fix: "A throw costs microseconds, about a thousand times a normal return, and with older toolchains the unwinder takes a global lock, so threads queue behind each other. Keep exceptions for rare errors: on a hot path return an error code, std::optional or std::expected, and check before the call that would throw.",
        ..R
    },
    // ------------------------------------------------------ C, C++, Rust
    Rule {
        within: In::C,
        id: "alloc",
        under: &[MALLOC],
        means: "allocating and freeing memory: malloc, free, operator new and delete",
        fix: "Allocate less on the hot path: reuse buffers and objects, reserve() vectors and strings up front, pass by reference or view (const&, std::string_view, std::span) instead of copying, move instead of copy, and use an arena or pool for many small objects that die together. Without changing code, try a faster allocator: LD_PRELOAD jemalloc, tcmalloc or mimalloc, which helps most with several threads.",
        ..R
    },
    Rule {
        id: "alloc",
        under: &[MALLOC, RUST_ALLOC],
        means: "allocating and freeing memory: Box, Vec, String and the collections growing, being cloned and dropped",
        fix: "Allocate less on the hot path: reuse a Vec or String across iterations (clear() keeps its capacity), give collections their size (with_capacity), borrow (&str, &[T]) instead of clone(), to_string() or collect() into a temporary, and keep small values inline (SmallVec, arrays). As a one-line change, set a faster global allocator (mimalloc or jemalloc): the allocator of musl targets is especially slow with threads.",
        ..RUST
    },
    Rule {
        within: In::Native,
        id: "copying",
        under: &[&[
            "=memcpy", "=memmove", "=memset", "=bzero", "=__bzero", "^__memcpy", "^__memmove", "^__mempcpy",
            "^__memset", "^__GI_memcpy", "^__GI_memmove", "^__GI_memset", "^_platform_memmove", "^_platform_memset",
            "^_platform_bzero",
        ]],
        means: "copying and clearing memory: memcpy, memmove and memset",
        fix: "Large values are copied, or big buffers cleared, again and again. Pass and keep references instead of copies, move instead of clone, grow containers once (reserve, with_capacity) so they are not copied each time they grow, and clear only the part of a buffer that was used. If one big copy is the job itself, there is little to gain.",
        ..R
    },
    Rule {
        within: In::C,
        id: "c-strings",
        under: &[&[
            "=strlen", "^__strlen", "=strnlen", "^__strnlen", "=strcmp", "^__strcmp", "=strncmp", "^__strncmp",
            "=strcasecmp", "^__strcasecmp", "=strchr", "^__strchr", "=strrchr", "^__strrchr", "=strstr", "^__strstr",
            "=strcpy", "^__strcpy", "=strncpy", "^__strncpy", "=strcat", "^__strcat", "=strdup", "=__strdup",
            "=memchr", "^__memchr", "^__rawmemchr", "^_platform_strlen", "^_platform_strcmp", "^_platform_strncmp",
            "^_platform_strchr", "^_platform_memchr", "^_platform_strcpy", "^_platform_strstr",
        ]],
        means: "scanning C strings: strlen, strcmp, strchr, strcpy and their kin",
        fix: "Each call walks the string to its end. strlen in a loop condition and strcat in a loop are quadratic: keep the length next to the pointer (std::string_view in C++), append at a known end, compare lengths before contents, and look names up in a hash table instead of a chain of strcmp.",
        ..R
    },
    Rule {
        within: In::C,
        id: "stdio",
        under: &[&[
            "=printf", "=fprintf", "=sprintf", "=snprintf", "=vfprintf", "=vsnprintf", "^__vfprintf", "^_IO_vfprintf",
            "^__vsnprintf", "^___snprintf_chk", "^__sprintf_chk", "^__printf_chk", "^__fprintf_chk", "=sscanf",
            "=fscanf", "=scanf", "^__vfscanf", "^_IO_vfscanf", "^__isoc99_", "^__isoc23_", "^__svfscanf", "=fputc",
            "=putc", "=putchar", "=fgetc", "=getc", "=getchar", "=_IO_putc", "=_IO_getc", "=fflush", "=_IO_fflush",
            "=fputs", "=_IO_fputs", "=puts", "=_IO_puts", "=fgets", "=_IO_fgets",
        ]],
        unless: &[
            "^std::num_put", "^std::num_get", "^std::basic_ostream", "^std::basic_istream", "^std::ostream::",
            "^std::istream::", "^std::operator<<", "^std::operator>>", "^std::__ostream_insert", "^std::to_string",
        ],
        means: "C stdio: printf and scanf formatting, getc and putc one character at a time, fflush",
        fix: "Format less in the hot loop and write bytes in bulk (fwrite, or build the line yourself). sscanf measures its whole input on every call, which is quadratic on a large buffer: parse with strtol or strtod and advance a pointer. getc and putc lock the stream on every call: use getc_unlocked and putc_unlocked, or fread and fwrite. Do not fflush after every line.",
        ..R
    },
    Rule {
        id: "iostream",
        under: &[&[
            "^std::basic_ostream", "^std::ostream::", "^std::basic_istream", "^std::istream::", "^std::operator<<",
            "^std::operator>>", "^std::__ostream_insert", "^std::endl", "^std::flush", "^std::getline",
            "^std::basic_stringbuf", "^std::basic_stringstream", "^std::basic_ostringstream",
            "^std::basic_istringstream", "^std::basic_filebuf", "^std::basic_streambuf", "^std::basic_ios",
            "^std::ios_base", "^std::locale", "^std::num_put", "^std::num_get", "^std::use_facet", "^std::ctype",
            "^std::__basic_file", "^std::__put_character_sequence",
        ]],
        means: "C++ iostreams: formatted << and >>, stringstream, and the locale work under them",
        fix: "std::endl flushes on every line: write '\\n'. For bulk cin and cout, call std::ios::sync_with_stdio(false) and std::cin.tie(nullptr) once. Do not build or parse numbers through a stringstream in a loop: use std::to_chars and std::from_chars, or std::format.",
        ..CXX
    },
    Rule {
        id: "regex",
        under: &[&[
            "^std::__detail::_Executor", "^std::__detail::_Compiler", "^std::__detail::_NFA", "^std::__detail::_Scanner",
            "^std::__detail::_BracketMatcher", "^std::__detail::_RegexTranslator", "^std::__detail::__regex_algo_impl",
            "^std::basic_regex", "^std::regex_", "^std::__match_", "^std::__loop", "^std::__alternate",
            "^std::__bracket_expression", "^std::__back_ref", "^std::__owns_", "^std::__lookahead", "^std::__repeat_one_loop",
        ]],
        means: "std::regex",
        fix: "std::regex is slow, often 10 to 100 times slower than other engines. Build each std::regex once, never per call. On a hot path replace it: RE2, PCRE2 with its JIT, Boost.Regex or CTRE, or plain string searches (find, starts_with) for fixed text.",
        ..CXX
    },
    Rule {
        id: "node-containers",
        under: &[&[
            "^std::_Rb_tree", "^std::__tree", "^std::_Hashtable", "^std::__hash_table", "^std::__detail::_Map_base",
            "^std::__detail::_Hash_node", "^std::__detail::_Hashtable", "^std::__detail::_Insert",
            "^std::__detail::_Prime_rehash", "^std::map<", "^std::set<", "^std::multimap<", "^std::unordered_map<",
            "^std::unordered_set<",
        ]],
        means: "std::map, std::set and the unordered containers: an allocation per element and a pointer to chase per step",
        fix: "For std::unordered_map, reserve() the size and check that the hash is cheap; a flat hash map (absl::flat_hash_map, boost::unordered_flat_map, ankerl::unordered_dense) is usually 2 to 5 times faster and nearly a drop-in. For small or mostly-read sorted data, a sorted std::vector with binary search (std::flat_map in C++23) beats std::map. Look a key up once, not find() and then operator[].",
        ..CXX
    },
    Rule {
        id: "strings",
        under: &[&["^std::basic_string"]],
        means: "std::string: building, copying and growing strings",
        fix: "Strings are built or copied on the hot path. Take std::string_view parameters instead of const std::string& built from literals or substr(), reserve() before appending in a loop, append in place (+=) instead of a + b + c, and move or return instead of copying.",
        ..CXX
    },
    Rule {
        id: "shared-ptr",
        under: &[&[
            "^std::_Sp_counted_base", "^std::__shared_count", "^std::__weak_count", "^std::__shared_weak_count",
            "^std::__shared_ptr", "^std::shared_ptr", "^std::weak_ptr", "^std::__weak_ptr",
        ]],
        means: "copying and releasing std::shared_ptr: atomic reference counts",
        fix: "Each copy is an atomic increment and each destruction a decrement, and threads sharing one pointer fight over that cache line. Pass const std::shared_ptr<T>& or a plain reference when the callee does not keep ownership, std::move at the last use, and use std::unique_ptr where there is one owner.",
        ..CXX
    },
    Rule {
        id: "dynamic-cast",
        under: &[&[
            "=__dynamic_cast", "^__cxxabiv1::__vmi_class_type_info", "^__cxxabiv1::__si_class_type_info",
            "^__cxxabiv1::__class_type_info",
        ]],
        means: "dynamic_cast: walking the class hierarchy and comparing type names",
        fix: "On a hot path replace dynamic_cast with a virtual function, a type tag and static_cast, or std::variant with std::visit.",
        ..CXX
    },
    // -------------------------------------------------------------- Rust
    Rule {
        id: "siphash",
        under: &[&[
            "~core::hash::sip::", "~core::hash::SipHasher", "~std::hash::random::",
            "~std::collections::hash::map::DefaultHasher", "~std::collections::hash::map::RandomState",
        ]],
        means: "hashing with SipHash, the default hasher of HashMap and HashSet",
        fix: "SipHash resists hash-flooding attacks but is slow, most of all for integers and short strings. Where the keys do not come from an attacker, switch the hasher: FxHashMap (rustc-hash), ahash or foldhash. It is a change of type, not of logic. Also look each key up once (the entry API) instead of contains_key and then insert.",
        ..RUST
    },
    Rule {
        id: "formatting",
        under: &[&[
            "~core::fmt::", "~alloc::fmt::format", " as core::fmt::Display>::fmt", " as core::fmt::Debug>::fmt",
        ]],
        means: "formatting: format!, to_string(), write! and the Display and Debug impls behind them",
        fix: "The formatting machinery is slow, and format! allocates a String each time. Keep it out of hot loops: do not build keys or compare values through to_string(), write into one reused String with write!, use itoa or ryu for numbers, and check that log and trace calls on the path do not format arguments for a level that is off.",
        ..RUST
    },
    Rule {
        id: "cloning",
        under: &[&[
            " as core::clone::Clone>::clone", "^core::clone::Clone::clone", "ToOwned>::to_owned",
            "ToOwned for str>::to_owned", "ToOwned for [T]>::to_owned", "^alloc::slice::<impl [T]>::to_vec",
            "ConvertVec>::to_vec",
            // to_string() on a String or a &str is a copy, not formatting.
            "=<alloc::string::String as alloc::string::SpecToString>::spec_to_string",
            "=<str as alloc::string::SpecToString>::spec_to_string",
        ]],
        means: "cloning: copies of Strings, Vecs and other owned values",
        fix: "Borrow instead of cloning (&str, &[T], &T), move the value at its last use, share big read-only data behind Arc or Rc, and use Cow where a copy is needed only sometimes. A clone() inside a loop, or in a closure called per item, is the usual culprit.",
        ..RUST
    },
    Rule {
        id: "stdout",
        under: &[&["~std::io::stdio::"]],
        means: "printing through std's stdout: a lock and a flush for every line",
        fix: "println! locks stdout on every call and flushes at every newline. Lock once and buffer: let mut out = std::io::BufWriter::new(std::io::stdout().lock()); then writeln!(out, ...). eprintln! is not buffered at all.",
        ..RUST
    },
    Rule {
        id: "regex-compile",
        under: &[&[
            "regex::regex::string::Regex::new", "regex::regex::bytes::Regex::new", "regex::builders::", "regex_syntax::",
            "regex_automata::meta::regex::Builder", "regex_automata::meta::strategy::new",
            "regex_automata::nfa::thompson::compiler", "regex::re_unicode::Regex::new", "regex::re_builder", "regex::compile::",
        ]],
        means: "compiling regular expressions at run time (Regex::new)",
        fix: "Regex::new is expensive and is being called on the hot path: in a loop, or in a function called per item. Build each regex once and reuse it: a static std::sync::LazyLock<Regex> (or once_cell, lazy_static). For fixed text use str::contains, starts_with or the memchr crate instead of a regex.",
        ..RUST
    },
    Rule {
        id: "mutex",
        cats: &[Cat::Lock],
        under: &[&[
            "~std::sync::mutex", "~std::sync::poison::mutex", "~std::sys::sync::mutex", "~std::sync::rwlock",
            "~std::sync::poison::rwlock", "~std::sys::sync::rwlock", "~parking_lot::raw_mutex",
            "~parking_lot::raw_rwlock", "~lock_api::mutex", "~lock_api::rwlock", "lock_contended",
        ]],
        means: "queued on a Mutex or RwLock",
        fix: "Hold the lock for less: compute outside it and lock only to read or swap the result. Split one lock into several (shards, DashMap, per-thread data merged at the end), use RwLock where reads dominate, atomics for counters, or hand ownership over a channel. In async code never hold a std Mutex across an .await.",
        ..RUST
    },
    // ---------------------------------------------------------------- Go
    Rule {
        id: "alloc-gc",
        under: &[&[
            "^runtime.mallocgc", "^runtime.newobject", "^runtime.newarray", "^runtime.makeslice", "^runtime.growslice",
            "^runtime.makemap", "^runtime.makechan", "^runtime.rawstring", "^runtime.rawbyteslice",
            "^runtime.slicebytetostring", "^runtime.stringtoslicebyte", "^runtime.concatstring", "^runtime.convT",
            "^runtime.gcBgMarkWorker", "^runtime.gcDrain", "^runtime.gcAssistAlloc", "^runtime.scanobject",
            "^runtime.scanblock", "^runtime.greyobject", "^runtime.markroot", "^runtime.gcMark", "^runtime.gcStart",
            "^runtime.gcSweep", "^runtime.bgsweep", "^runtime.sweepone", "^runtime.(*sweepLocked)",
            "^runtime.bgscavenge", "^runtime.(*mheap).alloc", "^runtime.(*mcache).refill", "^runtime.(*mcentral)",
            "^runtime.(*mspan)", "^runtime.(*gcWork)", "^runtime.wbBufFlush", "^runtime.gcWriteBarrier",
        ]],
        means: "allocating memory and collecting garbage",
        fix: "Allocate less on the hot path: the collector's work follows the allocation rate. Give slices and maps their size (make with a capacity), reuse buffers (sync.Pool, or one buffer reset per iteration), and avoid []byte to string conversions, boxing values into interfaces, and pointers where a value will do; go build -gcflags=-m shows what escapes to the heap. As a setting, GOGC=200 or more trades memory for fewer collections; set GOMEMLIMIT with it to bound the heap.",
        ..GO
    },
    Rule {
        id: "maps",
        under: &[&[
            "^runtime.mapaccess", "^runtime.mapassign", "^runtime.mapiter", "^runtime.mapdelete",
            "^internal/runtime/maps.", "^runtime.(*hmap)", "^runtime.evacuate", "^runtime.hashGrow", "^runtime.strhash",
            "^runtime.memhash", "^runtime.aeshash",
        ]],
        means: "map operations: hashing keys, lookups, inserts and growth",
        fix: "Size maps up front (make(map[K]V, n)) so they do not rehash while growing, look a key up once and keep the result, and use cheaper keys: an integer or a small struct instead of a string built for the lookup. A small fixed set of keys is faster as a slice or a switch. Go 1.24 made the built-in map faster: upgrading helps on its own.",
        ..GO
    },
    Rule {
        id: "json",
        under: &[&["^encoding/json."]],
        means: "encoding/json: JSON encoding and decoding driven by reflection",
        fix: "encoding/json inspects types by reflection and allocates as it goes. Decode into typed structs, not map[string]any; do not encode and decode the same data twice; reuse encoders and buffers. For a hot path use a generated or faster codec: easyjson, goccy/go-json, bytedance/sonic, or encoding/json/v2 (GOEXPERIMENT=jsonv2 in Go 1.25).",
        ..GO
    },
    Rule {
        id: "reflection",
        under: &[&["^reflect."]],
        below: Below::Runtime,
        means: "reflection (package reflect): inspecting types and values at run time",
        fix: "Reflection is many times slower than direct code, and it allocates. It usually comes from a library called per item: encoding/json, fmt, text/template, an ORM or a validator. Call it less (once per batch, cache the result), or replace it on the hot path with generated code, generics or a type switch.",
        ..GO
    },
    Rule {
        id: "regex",
        under: &[&["^regexp.", "^regexp/syntax."]],
        means: "regular expressions (package regexp)",
        fix: "Go's regexp runs in guaranteed linear time but is slow in absolute terms. Compile once at package level (regexp.MustCompile), never inside the function or loop: time under regexp/syntax or regexp.compile means a pattern is compiled again and again. For fixed text use strings.Contains, HasPrefix, Cut or Index, which are far faster.",
        ..GO
    },
    Rule {
        id: "fmt",
        under: &[&["^fmt."]],
        below: Below::NotYours,
        means: "fmt: Sprintf, Fprintf and their kin",
        fix: "fmt inspects each argument by reflection and boxes it into an interface. On a hot path use strconv (Itoa, AppendInt, FormatFloat), concatenation or a strings.Builder, and build log lines only when the level is on (log/slog with typed fields avoids fmt).",
        ..GO
    },
    Rule {
        id: "cgo",
        under: &[&["=runtime.cgocall", "^runtime.asmcgocall"]],
        means: "calls from Go into C (cgo), and what the C code does there",
        fix: "A cgo call costs far more than a Go call and ties up a thread. If the time is in the C library itself, that library is the work. If it is in crossing over (runtime.cgocall with little under it), make fewer, bigger calls: pass a batch across the boundary instead of one item at a time.",
        ..GO
    },
    Rule {
        id: "scheduler",
        under: &[&[
            "^runtime.schedule", "^runtime.findRunnable", "^runtime.findrunnable", "^runtime.stealWork",
            "^runtime.park_m", "^runtime.gopark", "^runtime.goready", "^runtime.ready", "^runtime.wakep",
            "^runtime.startm", "^runtime.stopm", "^runtime.futex", "^runtime.notewakeup", "^runtime.notesleep",
            "^runtime.runqgrab", "^runtime.runqsteal", "^runtime.lock2", "^runtime.unlock2", "^runtime.procyield",
            "^runtime.osyield", "^runtime.usleep", "^runtime.netpoll", "^runtime.chansend", "^runtime.chanrecv",
            "^runtime.selectgo", "^runtime.mcall", "^runtime.goschedImpl", "^runtime.Gosched", "^runtime.newproc",
            "^runtime.resetspinning", "^runtime.semasleep", "^runtime.semawakeup", "^runtime.pthread_cond",
        ]],
        min: 0.1,
        means: "the Go scheduler: parking and waking goroutines, channel operations, looking for work",
        fix: "In a program that is mostly idle this is only the cost of waking up for each event. In a busy one, goroutines are handing each other tiny pieces of work: send batches over channels instead of single items, give channels a buffer, start a bounded pool of workers instead of a goroutine per item (errgroup.SetLimit), and guard simple shared state with a mutex or an atomic instead of a channel. In a container before Go 1.25, set GOMAXPROCS to the CPU limit (or import go.uber.org/automaxprocs): the default is every core of the host.",
        ..GO
    },
    Rule {
        id: "mutex",
        // A contended lock spins before it parks the goroutine, and a
        // parked goroutine holds no thread: most of it shows as CPU.
        cats: &[Cat::Lock, Cat::Cpu],
        under: &[&[
            "^sync.(*Mutex).lockSlow", "^sync.(*Mutex).unlockSlow", "^internal/sync.(*Mutex).lockSlow",
            "^internal/sync.(*Mutex).unlockSlow", "^sync.(*RWMutex).rUnlockSlow", "^sync.runtime_SemacquireMutex",
            "^sync.runtime_SemacquireRWMutex", "^sync.runtime_doSpin",
        ]],
        means: "a contended sync.Mutex or RWMutex: goroutines spinning and queueing for the lock",
        fix: "Shorten what runs under the lock: no I/O, no heavy allocation and no calls out while holding it. Split one lock into several (per shard or per key), use RWMutex only when reads really dominate and are not tiny, atomics for counters and flags, and sync.Map for caches written once and read often. The mutex profile (runtime.SetMutexProfileFraction and pprof) names the contended lock.",
        ..GO
    },
    // ------------------------------------------------------------ Python
    Rule {
        id: "interpreter",
        top: &[
            "^_PyEval_", "=_PyEvalFramePushAndInit", "=_PyFunction_Vectorcall", "=PyObject_Vectorcall",
            "=_PyObject_MakeTpCall", "=_PyObject_Call", "=PyObject_Call", "^method_vectorcall", "^slot_tp_",
            "=PyObject_GetAttr", "=PyObject_SetAttr", "^_PyObject_GenericGetAttrWithDict",
            "^_PyObject_GenericSetAttrWithDict", "=PyObject_GenericGetAttr", "^_PyType_Lookup", "^_Py_dict_lookup",
            "^_PyDict_GetItem", "^PyDict_GetItem", "^_PyDict_LoadGlobal", "^unicodekeys_lookup", "^PyNumber_",
            "^long_", "^float_", "=PyObject_RichCompare", "=do_richcompare", "^PyObject_IsTrue", "^PyObject_GetItem",
            "^PyObject_SetItem", "^_PyFrame_", "^_Py_CheckFunctionResult",
        ],
        means: "the Python interpreter itself: running bytecode, calling functions, looking up attributes and names",
        fix: "The time is in Python-level code, not in a C library: a loop that runs too many times in the interpreter. Move the inner loop into C with built-ins and the standard library (sum, min, sorted, str.join, comprehensions, itertools, collections), vectorise numeric work with NumPy, cache repeated results (functools.lru_cache), and use a dict or a set instead of scanning a list. Python 3.11 and later run such code 10 to 60% faster than 3.10.",
        ..PY
    },
    Rule {
        id: "gc",
        under: &[&[
            "^gc_collect", "=deduce_unreachable", "=move_unreachable", "=subtract_refs", "=update_refs",
            "=visit_reachable", "=visit_decref", "=visit_move", "^_PyGC_", "^PyGC_Collect", "^_Py_RunGC", "^gc_mark_",
            "^mark_alive", "=handle_weakrefs", "=delete_garbage", "=finalize_garbage",
        ]],
        means: "the cyclic garbage collector",
        fix: "It runs each time enough container objects have been created, and a full pass walks every tracked object, so building big structures makes it run often and long. Around a bulk load or build, call gc.disable() and gc.enable() after; in a long-lived worker call gc.freeze() once start-up is done; or raise the thresholds with gc.set_threshold. Fewer, flatter objects (tuples, __slots__, arrays, NumPy) leave less to walk.",
        ..PY
    },
    Rule {
        id: "objects",
        under: &[&[
            "^_PyObject_Malloc", "^PyObject_Malloc", "^_PyObject_Free", "=PyObject_Free", "^_PyObject_Realloc",
            "=PyObject_Realloc", "^PyMem_Malloc", "^PyMem_Free", "^PyMem_Realloc", "^PyMem_Raw", "^_PyMem_",
            "=pymalloc_alloc", "=pymalloc_free", "^_PyObject_GC_New", "^_PyObject_New", "^PyObject_GC_Del",
            "=_Py_Dealloc", "^subtype_dealloc", "^dict_dealloc", "^list_dealloc", "^tuple_dealloc", "^frame_dealloc",
        ]],
        means: "creating and freeing Python objects",
        fix: "The code makes many short-lived objects. Avoid temporaries in the hot loop: build strings with str.join instead of + in a loop, iterate with generators instead of building lists, give classes with many instances __slots__, and keep numbers in arrays or NumPy rather than in lists of objects.",
        ..PY
    },
    Rule {
        id: "gil",
        cats: &[Cat::Lock],
        under: &[&["=take_gil", "^PyEval_RestoreThread", "^PyEval_AcquireThread", "^_PyThreadState_Attach"]],
        means: "waiting for the GIL: only one thread runs Python code at a time",
        fix: "Threads do not speed up CPU work in Python: they take turns. Use processes for CPU-bound work (concurrent.futures.ProcessPoolExecutor, multiprocessing), keep threads for I/O, and prefer C extensions that release the GIL while they work (NumPy, hashlib, zlib, most database drivers). Fewer threads means less time handing the GIL over. The free-threaded build (python3.13t and later) removes it.",
        ..PY
    },
    Rule {
        id: "imports",
        under: &[&[
            "^PyImport_Import", "^_PyImport_", "=import_find_and_load", "^import_name", "=builtin___import__",
            "^_imp_", "^r_object", "^marshal_loads", "=PyMarshal_ReadObjectFromString", "#<frozen importlib",
        ]],
        means: "importing modules: finding files, loading bytecode and running each module's top level",
        fix: "This is start-up cost, and it dominates short runs: command-line tools, tests, serverless handlers. python -X importtime shows which imports cost most. Import heavy packages (pandas, torch, boto3) inside the function that needs them instead of at the top, avoid work at import time, and keep bytecode caching on (do not set PYTHONDONTWRITEBYTECODE).",
        ..PY
    },
    Rule {
        id: "regex",
        under: &[&[
            "^sre_ucs1_", "^sre_ucs2_", "^sre_ucs4_", "^_sre_", "^sre_search", "^sre_match", "=pattern_subx",
            "#/re/_parser.py", "#/re/_compiler.py", "#/sre_parse.py", "#/sre_compile.py",
        ]],
        means: "regular expressions (the re module)",
        fix: "If the time is in re/_parser.py or re/_compiler.py (sre_parse, sre_compile), patterns are compiled again and again: build them once with re.compile at module level (a pattern made with an f-string per call defeats re's cache). If it is in matching, the pattern backtracks: make it specific, anchor it, avoid nested quantifiers like (a+)+. For fixed text, str methods (in, startswith, split, partition, replace) are much faster.",
        ..PY
    },
    Rule {
        id: "json",
        under: &[&[
            "^scanner_call", "^scan_once_unicode", "^_parse_object_unicode", "^_parse_array_unicode",
            "^scanstring_unicode", "^_match_number_unicode", "^encoder_call", "^encoder_listencode_",
            "^encoder_encode_", "^py_encode_basestring", "^ascii_escape_unicode", "^py_scanstring", "@_json.",
            "#/json/decoder.py", "#/json/encoder.py", "#/json/scanner.py",
        ]],
        means: "JSON encoding and decoding (the json module)",
        fix: "Parse and serialise less: once per message, not once per layer, and do not round-trip through JSON to copy objects. For a hot path switch to orjson or msgspec, which are several times faster and close to drop-in. For big files, stream (ijson) or use a format with one record per line.",
        ..PY
    },
    Rule {
        id: "pickle",
        under: &[&["^_pickle_", "@_pickle.", "#/pickle.py", "#/multiprocessing/reduction.py"]],
        means: "pickling: copying Python objects between processes",
        fix: "multiprocessing, concurrent.futures and joblib pickle every argument and every result. Send less across: pass big inputs once (an initializer, shared memory, a file path or an index) instead of with every task, give map a larger chunksize so tasks travel in batches, and return small results.",
        ..PY
    },
    Rule {
        id: "tracing",
        under: &[&[
            "^CTracer_", "=trace_trampoline", "=profile_trampoline", "=call_trampoline", "^sys_trace_", "^sys_profile_",
            "^call_trace", "^maybe_call_line_trace", "=profiler_callback", "^ptrace_enter_call", "^ptrace_leave_call",
            "@tracer.cpython", "@_lsprof", "@pydevd", "@_pydevd", "#/coverage/", "#/bdb.py", "#/pdb.py", "#/cProfile.py",
            "#/pydevd",
        ]],
        below: Below::NotYours,
        means: "a tracer running inside the program: coverage, a debugger or a profiler calls a hook on every line or call",
        fix: "This is overhead added by the tool, not the program's own work. Measure without it: run tests without --cov, without a debugger attached and without cProfile. If coverage must stay on, Python 3.12 and later can collect it far more cheaply: set COVERAGE_CORE=sysmon.",
        ..PY
    },
    // ----------------------------------------------------------- Node.js
    Rule {
        id: "gc",
        under: &[&[
            "v8::internal::Heap::CollectGarbage", "v8::internal::Heap::PerformGarbageCollection",
            "v8::internal::Heap::Scavenge", "v8::internal::Heap::MarkCompact", "v8::internal::Heap::MinorMarkSweep",
            "v8::internal::Scavenger", "v8::internal::ScavengerCollector", "v8::internal::MarkCompactCollector",
            "v8::internal::MinorMarkSweepCollector", "v8::internal::ConcurrentMarking",
            "v8::internal::IncrementalMarking", "v8::internal::Sweeper", "v8::internal::MarkingVisitorBase",
            "v8::internal::Evacuator", "v8::internal::HeapAllocator::AllocateRawWith", "v8::internal::LocalHeap::",
        ]],
        means: "V8 garbage collection",
        fix: "The code allocates fast: objects, arrays, closures and strings made per item or per request. Avoid copies in loops (spread and concat building a new array or object each time, map().filter() chains over big arrays), reuse buffers, and stream instead of holding everything in memory. As a setting, a larger young generation often cuts scavenge time a lot: node --max-semi-space-size=64 (MB; the default is 16). Raise --max-old-space-size if the heap runs near its limit.",
        ..JS
    },
    Rule {
        id: "startup",
        under: &[&[
            "v8::internal::Compiler::", "v8::internal::parsing::", "v8::internal::Parser", "v8::internal::ParserBase",
            "v8::internal::PreParser", "v8::internal::Scanner", "v8::internal::interpreter::BytecodeGenerator",
            "v8::internal::interpreter::Interpreter", "v8::ScriptCompiler::", "v8::internal::ScriptCompiler",
            "v8::internal::OptimizedCompilationJob", "v8::internal::OptimizingCompileDispatcher",
            "v8::internal::compiler::", "v8::internal::maglev::", "v8::internal::baseline::",
            "v8::internal::CodeSerializer", "node::builtins::BuiltinLoader::LookupAndCompile",
            "node::contextify::ContextifyScript::New", "node::contextify::ContextifyContext::CompileFunction",
            "node::contextify::CompileFunction", "node::loader::ModuleWrap::New", "node::modules::",
        ]],
        // The program runs under the loader's own functions: only their own time.
        top: &["#node:internal/modules/"],
        means: "loading, parsing and compiling JavaScript: module resolution, V8's parser and its compilers",
        fix: "In a short run (a command-line tool, a test file, a cold start) this is start-up, and it is most of the time. Load less: require or import() heavy modules where they are used, and bundle the program into one file (esbuild) so there is less to resolve and read. Node 22 and later can keep compiled code between runs: set NODE_COMPILE_CACHE to a directory. Run TypeScript already compiled, not through ts-node. In a long-running process this fades after warm-up.",
        ..JS
    },
    Rule {
        id: "slow-properties",
        under: &[&[
            "IC_Miss", "IC_Megamorphic", "v8::internal::LoadIC::", "v8::internal::StoreIC::",
            "v8::internal::KeyedLoadIC::", "v8::internal::KeyedStoreIC::", "v8::internal::IC::",
            "v8::internal::Deoptimizer", "v8::internal::Runtime_NotifyDeoptimized", "Builtins_DeoptimizationEntry",
            "v8::internal::JSObject::MigrateToMap", "v8::internal::JSObject::NormalizeProperties",
            "v8::internal::Map::TransitionTo", "v8::internal::Runtime_GetProperty",
            "v8::internal::Runtime_SetKeyedProperty",
        ]],
        means: "slow property access and deoptimisation: V8's fast paths are missing",
        fix: "One piece of code sees objects of many different shapes, or optimised code keeps being thrown away. Create objects with the same properties in the same order (a class, a constructor or a factory), do not add or delete properties afterwards, keep an array to one kind of element, and use a Map for keys known only at run time. node --trace-deopt names the functions that deoptimise.",
        ..JS
    },
    Rule {
        id: "regex",
        under: &[&[
            "v8::internal::RegExp", "v8::internal::Runtime_RegExp", "v8::internal::NativeRegExpMacroAssembler",
            "v8::internal::IrregexpInterpreter", "v8::internal::regexp", "Builtins_RegExp", "^RegExp:",
            "v8::internal::Runtime_StringReplace", "v8::internal::Runtime_StringSplit",
        ]],
        means: "regular expressions",
        fix: "A pattern that backtracks takes time that grows steeply with its input: avoid nested quantifiers like (a+)+ and alternatives that overlap, and anchor the pattern. Create each RegExp once, outside the loop. For fixed text use includes, startsWith, indexOf, or split with a string. For patterns that come from users, a linear-time engine (the re2 package) rules backtracking out.",
        ..JS
    },
    Rule {
        id: "json",
        under: &[&[
            "v8::internal::JsonParser", "v8::internal::JsonStringifier", "v8::internal::JsonStringify",
            "v8::internal::JsonParseInternalizer", "v8::internal::Builtin_JsonParse",
            "v8::internal::Builtin_JsonStringify", "Builtins_JsonParse", "Builtins_JsonStringify",
        ]],
        means: "JSON.parse and JSON.stringify",
        fix: "They run on the main thread, so a large document holds up every other request while it is parsed. Parse and serialise once, not at every layer; do not deep-copy with JSON.parse(JSON.stringify(x)); send smaller payloads (paginate, select fields); stream very large documents; and for responses of a known shape, a serialiser compiled from a schema (fast-json-stringify) is faster.",
        ..JS
    },
    // --------------------------------------------------------------- JVM
    Rule {
        id: "gc",
        under: &[&[
            "G1ParScanThreadState", "G1CollectedHeap", "G1ConcurrentMark", "G1CMTask", "G1RemSet", "G1YoungCollector",
            "G1FullCollector", "G1FullGC", "G1EvacuateRegions", "G1ParEvacuate", "G1ParCopy", "G1ScanEvacuated",
            "G1Policy", "G1ConcurrentRefine", "G1BarrierSet", "PSPromotionManager", "PSScavenge",
            "ParallelScavengeHeap", "PSParallelCompact", "ParCompactionManager", "PSOldGen", "PSYoungGen", "ZMark",
            "ZHeap", "ZRelocate", "ZDriver", "ZCollectedHeap", "ZGeneration", "ZBarrier", "Shenandoah", "SerialHeap",
            "DefNewGeneration", "TenuredGeneration", "GenCollectedHeap", "MarkSweep", "ConcurrentGCThread",
            "VM_GenCollect", "VM_G1", "VM_ParallelGC", "VM_CollectForAllocation", "MemAllocator::", "CollectedHeap::",
            "InstanceKlass::allocate_instance", "TypeArrayKlass::allocate", "ObjArrayKlass::allocate",
            "OptoRuntime::new_instance_C", "OptoRuntime::new_array_C", "OptoRuntime::new_array_nozero_C",
            "Runtime1::new_instance", "Runtime1::new_type_array", "Runtime1::new_object_array",
            "ThreadLocalAllocBuffer",
        ]],
        means: "garbage collection, and allocation that did not fit the fast path",
        fix: "Allocate less on the hot path: boxed numbers in collections (Map<Integer, Long>), streams and lambdas in inner loops, String.format and concatenation in loops, and temporary collections. Then give the collector room: its cost climbs steeply when live data nears -Xmx (-Xlog:gc shows how often it runs). In a container the default heap is a quarter of the memory limit: set -XX:MaxRAMPercentage=75. For batch work, -XX:+UseParallelGC gives the most throughput.",
        ..JVM
    },
    Rule {
        id: "warm-up",
        under: &[&[
            "CompileBroker", "C2Compiler", "Compile::", "PhaseChaitin", "PhaseIdealLoop", "PhaseIterGVN", "PhaseCCP",
            "PhaseMacroExpand", "PhaseOutput", "PhaseCFG", "Matcher::", "Compilation::", "LinearScan", "GraphBuilder",
            "JVMCI", "CompilerThread", "ciEnv::", "ciMethod", "SystemDictionary::", "ClassFileParser", "ClassLoader::",
            "KlassFactory", "InstanceKlass::link_class", "InstanceKlass::initialize", "LinkResolver::", "Rewriter::",
            "InterpreterRuntime::", "java.lang.ClassLoader", "jdk.internal.loader.", "java.util.zip.", "java.util.jar.",
        ]],
        top: &["=Interpreter"],
        below: Below::NotYours,
        means: "JVM start-up and warm-up: loading classes, running in the interpreter and JIT-compiling",
        fix: "Expected in the first seconds, and most of a short run. For short-lived programs and tests: reuse a warm JVM (a daemon or a server, as Gradle and Maven do), load classes from an archive (-XX:+AutoCreateSharedArchive -XX:SharedArchiveFile=app.jsa on JDK 19 and later), stop the JIT at its fast tier (-XX:TieredStopAtLevel=1), or build a native image. For a service, measure after warm-up; for a benchmark, use JMH.",
        ..JVM
    },
    Rule {
        id: "exceptions",
        under: &[&[
            "java.lang.Throwable.fillInStackTrace", "java/lang/Throwable;::fillInStackTrace",
            "java_lang_Throwable::fill_in_stack_trace", "JVM_FillInStackTrace", "Exceptions::_throw",
            "OptoRuntime::handle_exception_C", "Runtime1::handle_exception_from_callee",
            "SharedRuntime::raw_exception_handler_for_return_address", "InterpreterRuntime::exception_handler_for_exception",
            "InterpreterRuntime::throw_", "InterpreterRuntime::create_exception",
        ]],
        means: "throwing exceptions: most of the cost is filling in the stack trace",
        fix: "An exception is used as ordinary control flow on a hot path. Return a value instead (Optional, a result type, a sentinel), or check first (containsKey, a parse that reports failure). Where an exception type must be thrown often, give it no stack trace: the constructor with writableStackTrace=false, or override fillInStackTrace.",
        ..JVM
    },
    Rule {
        id: "reflection",
        under: &[&[
            "java.lang.reflect.", "java/lang/reflect/", "jdk.internal.reflect.", "jdk/internal/reflect/", "sun.reflect.",
            "Reflection::invoke", "JVM_InvokeMethod", "JVM_NewInstanceFromConstructor", "java.lang.Class.forName",
            "java.lang.Class.getMethod", "java.lang.Class.getDeclared", "java.beans.Introspector",
        ]],
        below: Below::Runtime,
        means: "reflection: looking classes and methods up, and the machinery of calling through them",
        fix: "Look classes, methods and fields up once and keep them: the lookups (Class.forName, getMethod, getDeclaredFields) cost far more than the calls. On a hot path replace per-call reflection, as in bean mappers and reflective serialisers, with generated code, a MethodHandle held in a static final field, or a direct call.",
        ..JVM
    },
    Rule {
        id: "regex",
        under: &[&["java.util.regex.", "java/util/regex/"]],
        means: "regular expressions (java.util.regex)",
        fix: "String.matches, replaceAll, replaceFirst and split with a regex compile the pattern on every call: keep a static final Pattern and reuse it. If the time is in matching rather than in Pattern.compile, the pattern backtracks: use possessive quantifiers or atomic groups, or plain string methods (indexOf, startsWith, contains) for fixed text.",
        ..JVM
    },
    Rule {
        id: "formatting",
        under: &[&[
            "java.util.Formatter", "java/util/Formatter", "java.lang.String.format", "java.text.MessageFormat",
            "java.text.SimpleDateFormat", "java.text.DecimalFormat", "java.text.NumberFormat",
        ]],
        means: "String.format and java.text formatting",
        fix: "String.format parses its format string on every call and is many times slower than concatenation. On a hot path concatenate or use a StringBuilder; for dates share one DateTimeFormatter (it is thread-safe) instead of creating a SimpleDateFormat each time; and let the logger do the formatting (log.debug(\"x={}\", x)) so nothing is built when the level is off.",
        ..JVM
    },
    Rule {
        id: "monitor",
        cats: &[Cat::Lock],
        under: &[&[
            "ObjectMonitor::enter", "ObjectMonitor::EnterI", "ObjectSynchronizer::",
            "SharedRuntime::complete_monitor_locking", "java.util.concurrent.locks.ReentrantLock",
            "java.util.concurrent.locks.ReentrantReadWriteLock", "java.util.concurrent.locks.StampedLock",
            "java/util/concurrent/locks/ReentrantLock", "java/util/concurrent/locks/ReentrantReadWriteLock",
            "java/util/concurrent/locks/StampedLock",
        ]],
        means: "queued on a monitor (synchronized) or a java.util.concurrent lock",
        fix: "Shrink what runs inside synchronized: no I/O, no logging and no calls into other components while holding the lock. Replace a shared synchronized collection or counter with ConcurrentHashMap, LongAdder or an atomic; use a ReadWriteLock or StampedLock when reads dominate; split one lock into several. jcmd <pid> Thread.print shows which thread holds the monitor.",
        ..JVM
    },
    // -------------------------------------------------------------- .NET
    Rule {
        id: "gc",
        under: &[&[
            "gc_heap::", "GCHeap::", "WKS::", "SVR::", "GCToEEInterface", "^JIT_New", "^JIT_Box", "^RhpNew",
            "^RhpGcAlloc", "AllocateObject", "AllocateSzArray", "SlowAllocateString", "FastAllocateString",
            "@libclrgc",
        ]],
        means: "garbage collection and allocation",
        fix: "Allocate less on the hot path: Span<T> and stackalloc for short-lived buffers, ArrayPool<T>.Shared for big ones, structs for small values, StringBuilder or string.Create instead of repeated concatenation, and no LINQ, closures or boxing in the inner loop. As a setting, Server GC gives more throughput on a multi-core server (DOTNET_gcServer=1 or <ServerGarbageCollector>true</ServerGarbageCollector>); a console program defaults to Workstation GC.",
        ..NET
    },
    Rule {
        id: "startup",
        under: &[&[
            "MethodDesc::DoPrestub", "^PreStubWorker", "UnsafeJitFunction", "CILJit::compileMethod", "@libclrjit",
            "MethodDesc::JitCompileCode", "MethodDesc::PrepareInitialCode", "MethodDesc::PrepareILBasedCode",
            "TieredCompilationManager", "ClassLoader::", "MethodTableBuilder",
        ]],
        means: ".NET start-up: JIT-compiling methods and loading types",
        fix: "Expected at start, and most of a short run. Publish with ReadyToRun (<PublishReadyToRun>true</PublishReadyToRun>) so code arrives compiled, or with Native AOT for command-line tools, and keep tiered compilation on (the default). For a service, measure after warm-up; for a benchmark, use BenchmarkDotNet.",
        ..NET
    },
    Rule {
        id: "exceptions",
        under: &[&[
            "DispatchManagedException", "^IL_Throw", "^IL_Rethrow", "ExceptionTracker::", "ProcessCLRException",
            "UnwindManagedExceptionPass", "StackTraceInfo::", "^RhpThrowEx", "^RhThrowEx", "System.Runtime.EH.",
            "System.Diagnostics.StackTrace", "RaiseTheExceptionInternalOnly", "AppendExceptionStackFrame",
        ]],
        means: "throwing and catching exceptions",
        fix: "A .NET exception costs tens of microseconds: it captures a stack trace and unwinds in two passes. Do not use one as ordinary control flow: use the Try pattern (int.TryParse, TryGetValue), return a result, and check before the call that would throw. dotnet-counters shows the exception rate.",
        ..NET
    },
    Rule {
        id: "linq",
        under: &[&["System.Linq."]],
        below: Below::NotYours,
        means: "LINQ: its iterators and the delegate called for each element",
        fix: "Each LINQ operator allocates an iterator and calls a delegate per element, and Count() followed by Any() or a second foreach walks the sequence again. In the hot path write the loop out over the array, List<T> or Span<T>, and materialise a query once (ToList, ToArray) if it is read several times.",
        ..NET
    },
    Rule {
        id: "regex",
        under: &[&["System.Text.RegularExpressions."]],
        means: "regular expressions (System.Text.RegularExpressions)",
        fix: "Build each Regex once and keep it in a static readonly field: new Regex(...) per call parses the pattern again. Use RegexOptions.Compiled, or better the [GeneratedRegex] source generator (.NET 7 and later). For patterns that backtrack on some inputs, RegexOptions.NonBacktracking guarantees linear time. For fixed text use string methods (Contains, StartsWith, IndexOf).",
        ..NET
    },
    Rule {
        id: "reflection",
        under: &[&[
            "System.Reflection.", "System.RuntimeMethodHandle", "RuntimeMethodHandle::InvokeMethod",
            "System.Activator.CreateInstance", "System.RuntimeType.", "System.ComponentModel.TypeDescriptor",
        ]],
        below: Below::Runtime,
        means: "reflection: looking types and members up, and the machinery of calling through them",
        fix: "Cache what reflection finds (MethodInfo, PropertyInfo) and turn repeated invocations into delegates (CreateDelegate, compiled expressions). Prefer source generators to reflection at run time: System.Text.Json's JsonSerializerContext, the logging and regex generators. Reuse one JsonSerializerOptions: a new one per call rebuilds its metadata every time.",
        ..NET
    },
    // -------------------------------------------------------------- Ruby
    Rule {
        id: "gc",
        under: &[&[
            "^gc_mark", "^gc_sweep", "^gc_start", "^gc_enter", "^gc_compact", "^rb_gc", "^garbage_collect",
            "^rgengc_", "^newobj_", "^rb_newobj", "^rb_wb_protected_newobj_of", "^rb_wb_unprotected_newobj_of",
            "^rb_ec_wb_protected_newobj_of", "^objspace_malloc", "^objspace_xmalloc", "^objspace_xfree",
            "^ruby_xmalloc", "^ruby_xfree", "^ruby_xrealloc", "^ruby_xcalloc", "^rb_objspace_", "^heap_page",
            "^heap_prepare", "^obj_free",
        ]],
        means: "garbage collection and object allocation",
        fix: "The code creates many short-lived objects. Add # frozen_string_literal: true, change objects in place (<<, map!, concat) instead of building new ones with + in a loop, avoid intermediate arrays (each_with_object, lazy, pluck instead of loading whole records), and reuse buffers. As a setting, a heap that grows faster is collected less often: RUBY_GC_HEAP_GROWTH_FACTOR and RUBY_GC_MALLOC_LIMIT (GC.stat shows what triggers collections).",
        ..RB
    },
    Rule {
        id: "interpreter",
        top: &[
            "=vm_exec_core", "=rb_vm_exec", "=vm_exec", "^vm_call_", "^vm_sendish", "^vm_search_method", "^vm_cc",
            "^vm_push_frame", "^vm_getivar", "^vm_setivar", "^vm_opt_", "^rb_vm_search_method_slowpath",
            "^rb_callable_method_entry", "^vm_invoke", "^invoke_block", "^vm_yield", "^rb_yield",
            "^vm_callee_setup", "^vm_caller_setup", "^rb_vm_invoke",
        ],
        means: "the Ruby interpreter itself: running bytecode and dispatching method calls",
        fix: "Turn on YJIT, Ruby's JIT compiler: ruby --yjit or RUBY_YJIT_ENABLE=1 (Ruby 3.2 and later), often 15 to 40% faster on such code. Then cut the work: let built-in methods do the looping (sum, tally, each_slice, group_by, pack) rather than hand-written each loops, memoise repeated calls, use a Hash or Set instead of Array#include?, and keep method_missing and send off hot paths.",
        ..RB
    },
    Rule {
        id: "regex",
        under: &[&["^onig_", "^onigenc_", "^rb_reg_", "^match_at"]],
        means: "regular expressions (Onigmo)",
        fix: "Use match? instead of =~ or match when you only need yes or no: it does not build a MatchData. A regex with interpolation (/#{x}/) is compiled on every evaluation: build it once, or add the o flag. For fixed text use start_with?, end_with?, include? and delete_prefix. If matching itself is slow, the pattern backtracks: simplify it (Ruby 3.2 and later cache much of this, and offer Regexp.timeout).",
        ..RB
    },
    Rule {
        id: "boot",
        under: &[&[
            "^rb_require", "^rb_f_require", "^require_internal", "^rb_load", "^load_iseq", "^rb_iseq_compile",
            "^rb_iseq_new", "^iseq_compile", "^rb_iseq_load", "^rb_parser_", "^ruby_yyparse", "^pm_parse", "^pm_iseq",
            "^prism_", "^rb_feature_", "^rb_find_file",
        ]],
        means: "loading Ruby files: searching the load path, parsing and compiling",
        fix: "This is boot time, and it is most of a short run: a rake task, a test file, a console. Bootsnap caches both the path lookups and the compiled code and usually halves it. Load fewer gems at boot (require: false in the Gemfile, then require where used), and in development and test do not eager-load the application.",
        ..RB
    },
    // ---------------------------------------------- Erlang and Elixir
    Rule {
        id: "gc",
        under: &[&[
            "^erts_garbage_collect", "^garbage_collect", "^erts_gc_", "^do_minor", "^minor_collection",
            "^major_collection", "^full_sweep", "^sweep_off_heap", "^delay_garbage_collection",
        ]],
        means: "garbage collection of Erlang process heaps",
        fix: "Each process collects its own heap, so one holding a lot of data collects slowly and often. Keep big state out of busy processes (ETS, or :persistent_term for data that rarely changes), build iodata instead of concatenating binaries, and start a short-lived worker that allocates a lot with a larger min_heap_size so it never collects. Binaries over 64 bytes are shared, not copied; a process that only passes them on can keep them alive: hibernate it or lower fullsweep_after.",
        ..BEAM
    },
    Rule {
        id: "copying",
        under: &[&["^copy_struct", "^copy_shallow", "^size_object", "^size_shared", "^erts_copy_", "^copy_object", "^erts_send_message"]],
        means: "copying terms between processes: messages, and reads and writes of ETS tables",
        fix: "A message is copied into the receiver's heap, and an ETS lookup copies the whole row out. Send less: a key or a reference instead of the data, only the fields needed, large payloads as binaries (shared above 64 bytes). Keep rows small and read single fields with :ets.lookup_element; put big, rarely changed data in :persistent_term, which is read without copying.",
        ..BEAM
    },
    Rule {
        id: "busy-wait",
        under: &[&["=scheduler_wait", "=sched_spin_wait", "^sched_prep_cont_spin_wait"]],
        means: "schedulers busy-waiting: spinning while they look for work, not doing any",
        fix: "The BEAM keeps idle schedulers spinning for a while so that they wake fast. It shows as CPU use but is not work. Turn it off with +sbwt none +sbwtdcpu none +sbwtdio none (in ERL_FLAGS, vm.args or the release's flags), at the price of slightly slower wake-ups. In a container, also check that the number of schedulers (+S) matches the CPU limit.",
        ..BEAM
    },
    Rule {
        id: "ets",
        under: &[&["^db_", "^ets_", "^erts_db_", "^erts_ets_"]],
        means: "ETS table operations",
        fix: "Look rows up by key: match and select without the key walk the whole table. Choose the table type for the access (set for key lookups, ordered_set for ranges), turn on read_concurrency or write_concurrency for tables shared by many processes, and use :ets.update_counter instead of read, change, write. Lookups copy the row: keep rows small.",
        ..BEAM
    },
];

/// The ids of the hints about one language.
#[cfg(test)]
pub fn ids(l: Lang) -> Vec<&'static str> {
    RULES.iter().filter(|r| r.lang == Some(l)).map(|r| r.id).collect()
}

fn matches(p: &str, name: &str, f: &Frame) -> bool {
    match p.as_bytes()[0] {
        b'=' => name == &p[1..],
        b'^' => name.starts_with(&p[1..]),
        b'@' => f.module.starts_with(&p[1..]),
        b'#' => f.file.as_deref().is_some_and(|x| x.contains(&p[1..])),
        b'%' => f.func.contains(&p[1..]),
        b'~' => {
            let own = name.strip_prefix('<').map(|n| n.strip_prefix("&mut ").or(n.strip_prefix('&')).unwrap_or(n));
            own.unwrap_or(name).starts_with(&p[1..])
        }
        _ => name.contains(p),
    }
}

/// Whether a frame that matched a rule may count for it: see `In`.
fn allowed(rule: &Rule, f: &Frame, own: Option<Lang>, at: Option<Lang>) -> bool {
    match rule.within {
        In::Any => true,
        // CPython's extension modules (_json, _pickle) are C, and Python's.
        In::Own => {
            own == rule.lang || rule.lang == Some(Lang::Python) && f.module.to_ascii_lowercase().contains("python")
        }
        In::Rust => at == Some(Lang::Rust),
        In::C => matches!(at, Some(Lang::C) | None),
        In::Native => matches!(at, Some(Lang::Rust) | Some(Lang::C) | None),
    }
}

/// A known cost found in the stacks.
#[derive(Clone, Debug)]
pub struct Hint {
    pub cat: Cat,
    pub ns: f64,
    /// Share of this cause's time in stacks that passed through it.
    pub share: f64,
    pub id: &'static str,
    /// The language whose guide says more.
    pub lang: Option<Lang>,
    /// The function most of that time was seen in.
    pub seen: String,
    pub whole: bool,
    pub means: &'static str,
    pub fix: &'static str,
}

#[derive(Default)]
struct Acc {
    ns: f64,
    seen: HashMap<usize, f64>,
    langs: HashMap<Option<Lang>, f64>,
}

fn most<K: Copy + Eq + std::hash::Hash>(m: &HashMap<K, f64>) -> Option<K> {
    m.iter().max_by(|a, b| a.1.total_cmp(b.1)).map(|x| *x.0)
}

/// The language of the program (`None` when nothing tells), and the known
/// costs in its stacks, largest first. `kinds` is `code::origin` of each
/// frame.
pub fn find(st: &Stacks, kinds: &[Kind]) -> (Option<Lang>, Vec<Hint>) {
    // What each frame says: its language (the C library and the kernel
    // are nobody's), and the rules its name shows.
    let langs: Vec<Option<Lang>> = st
        .frames
        .iter()
        .zip(kinds)
        .map(|(f, k)| match k {
            Kind::Sys | Kind::Kernel | Kind::Unknown => None,
            _ => lang::of(f),
        })
        .collect();
    let mut under: Vec<Vec<u16>> = vec![Vec::new(); st.frames.len()];
    let mut top: Vec<Vec<u16>> = vec![Vec::new(); st.frames.len()];
    let mut unless: Vec<Vec<u16>> = vec![Vec::new(); st.frames.len()];
    for (i, f) in st.frames.iter().enumerate() {
        if f.func == "[unknown]" {
            continue;
        }
        let name = lang::bare(f);
        for (r, rule) in RULES.iter().enumerate() {
            if rule.under.iter().any(|g| g.iter().any(|p| matches(p, &name, f))) {
                under[i].push(r as u16);
            }
            if rule.top.iter().any(|p| matches(p, &name, f)) {
                top[i].push(r as u16);
            }
            if rule.unless.iter().any(|p| matches(p, &name, f)) {
                unless[i].push(r as u16);
            }
        }
    }
    let mut total: HashMap<Cat, f64> = HashMap::new();
    // The program's language, by time; separately for idle time, which
    // decides only when there is nothing else.
    let mut prog: [HashMap<Lang, f64>; 2] = [HashMap::new(), HashMap::new()];
    let mut acc: HashMap<(usize, Cat), Acc> = HashMap::new();
    for s in &st.samples {
        // The root-most frame that tells: a Python program is one even
        // while it is inside a C extension.
        if let Some(l) = s.frames.iter().rev().find_map(|&i| langs[i]) {
            *prog[(s.cat == Cat::Idle) as usize].entry(l).or_default() += s.ns;
        }
        if s.cat == Cat::Idle {
            continue;
        }
        *total.entry(s.cat).or_default() += s.ns;
        // The leaf-most: the code the time is in.
        let at = s.frames.iter().find_map(|&i| langs[i]);
        let first = s.frames.iter().copied().find(|&i| !st.frames[i].kernel);
        let not: Vec<u16> = s.frames.iter().flat_map(|&i| unless[i].iter().copied()).collect();
        let mut hit: Vec<(u16, usize)> = Vec::new();
        let mut note = |r: u16, i: usize, ok: bool| {
            let rule = &RULES[r as usize];
            if ok
                && rule.cats.contains(&s.cat)
                && !not.contains(&r)
                && !hit.iter().any(|h| h.0 == r)
                && allowed(rule, &st.frames[i], langs[i], at)
            {
                hit.push((r, i));
            }
        };
        // From the leaf: whose code has been passed on the way up.
        let (mut yours, mut foreign) = (false, false);
        for &i in &s.frames {
            for &r in &under[i] {
                let ok = match RULES[r as usize].below {
                    Below::All => true,
                    Below::NotYours => !yours,
                    Below::Runtime => !foreign,
                };
                note(r, i, ok);
            }
            yours |= kinds[i] == Kind::App;
            foreign |= matches!(kinds[i], Kind::App | Kind::Lib);
        }
        if let Some(i) = first {
            for &r in &top[i] {
                note(r, i, true);
            }
        }
        for (r, i) in hit {
            let a = acc.entry((r as usize, s.cat)).or_default();
            a.ns += s.ns;
            *a.seen.entry(i).or_default() += s.ns;
            *a.langs.entry(at).or_default() += s.ns;
        }
    }
    let lang = most(&prog[0]).or_else(|| most(&prog[1]));
    let active: f64 = total.values().sum();
    let mut hints: Vec<Hint> = acc
        .into_iter()
        .filter_map(|((r, cat), a)| {
            let rule = &RULES[r];
            let t = total.get(&cat).copied().unwrap_or(0.0);
            // A cause that is a sliver of the time is not worth a hint.
            if t <= 0.0 || t / active < 0.1 || a.ns / t < rule.min {
                return None;
            }
            let seen = most(&a.seen).map(|i| lang::bare(&st.frames[i]).into_owned()).unwrap_or_default();
            Some(Hint {
                cat,
                ns: a.ns,
                share: a.ns / t,
                id: rule.id,
                lang: rule.lang.or_else(|| most(&a.langs).flatten()),
                seen: crate::out::shorten(&seen, 100),
                whole: rule.whole,
                means: rule.means,
                fix: rule.fix,
            })
        })
        .collect();
    // What concerns the whole program first, then by time.
    hints.sort_by(|a, b| b.whole.cmp(&a.whole).then(b.ns.total_cmp(&a.ns)).then(a.id.cmp(b.id)));
    hints.truncate(5);
    (lang, hints)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code::origin;

    fn fr(func: &str, module: &str) -> Frame {
        Frame {
            func: func.into(),
            module: module.into(),
            ..Frame::default()
        }
    }

    /// A stack: its cause, its seconds, and its frames from the leaf as
    /// (function, module).
    type Stack<'a> = (Cat, f64, &'a [(&'a str, &'a str)]);

    fn found(samples: &[Stack]) -> (Option<Lang>, Vec<Hint>) {
        let mut st = Stacks::new("test");
        for (cat, secs, frames) in samples {
            let ids = frames.iter().map(|(f, m)| st.intern(fr(f, m))).collect();
            st.add(1, 1, *cat, ids, secs * 1e9);
        }
        let kinds: Vec<Kind> = st.frames.iter().map(|f| origin(f).0).collect();
        find(&st, &kinds)
    }

    #[test]
    fn rules_are_well_formed() {
        let mut ids = std::collections::HashSet::new();
        for r in RULES {
            assert!(!r.id.is_empty() && !r.means.is_empty() && !r.fix.is_empty(), "{}", r.id);
            assert!(!r.under.is_empty() || !r.top.is_empty(), "{}", r.id);
            assert!(r.within != In::Own || r.lang.is_some(), "{}", r.id);
            assert!(r.means.len() < 160 && r.fix.len() < 700, "{}: too long to read in an answer", r.id);
            for p in r.under.iter().flat_map(|g| g.iter()).chain(r.top) {
                assert!(p.len() > 2, "{}: pattern '{p}' matches too much", r.id);
            }
            // One hint per language and cause under one name.
            for c in r.cats {
                assert!(ids.insert((r.lang, r.within == In::C, r.id, *c)), "{}: twice", r.id);
            }
        }
    }

    #[test]
    fn go_allocation_is_summed_over_call_sites() {
        let (lang, h) = found(&[
            (Cat::Cpu, 2.0, &[("runtime.mallocgc", "srv"), ("runtime.growslice", "srv"), ("main.parse", "srv"), ("runtime.goexit", "srv")]),
            (Cat::Cpu, 1.0, &[("runtime.scanobject", "srv"), ("runtime.gcBgMarkWorker", "srv"), ("runtime.goexit", "srv")]),
            (Cat::Cpu, 1.0, &[("runtime.memmove", "srv"), ("encoding/json.Marshal", "srv"), ("main.reply", "srv"), ("runtime.goexit", "srv")]),
            (Cat::Cpu, 4.0, &[("main.compute", "srv"), ("runtime.goexit", "srv")]),
        ]);
        assert_eq!(lang, Some(Lang::Go));
        assert_eq!(h[0].id, "alloc-gc");
        assert_eq!(h[0].lang, Some(Lang::Go));
        assert!((h[0].share - 3.0 / 8.0).abs() < 1e-9);
        assert_eq!(h[0].seen, "runtime.mallocgc");
        assert_eq!(h[1].id, "json");
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn python_without_its_function_names() {
        let (lang, h) = found(&[
            (Cat::Cpu, 6.0, &[("_PyEval_EvalFrameDefault", "python3.11"), ("_PyFunction_Vectorcall", "python3.11"), ("Py_BytesMain", "python3.11")]),
            (Cat::Cpu, 2.0, &[("deduce_unreachable", "python3.11"), ("gc_collect_main", "python3.11"), ("_PyEval_EvalFrameDefault", "python3.11"), ("Py_BytesMain", "python3.11")]),
            (Cat::Cpu, 2.0, &[("DOUBLE_add", "_multiarray_umath.cpython-311-x86_64-linux-gnu.so"), ("_PyEval_EvalFrameDefault", "python3.11"), ("Py_BytesMain", "python3.11")]),
            (Cat::Lock, 5.0, &[("pthread_cond_timedwait", "libc.so.6"), ("take_gil", "python3.11"), ("PyEval_RestoreThread", "python3.11")]),
        ]);
        assert_eq!(lang, Some(Lang::Python));
        let ids: Vec<(&str, Cat)> = h.iter().map(|x| (x.id, x.cat)).collect();
        // The loop itself, not the time under it in NumPy or the collector.
        assert_eq!(ids, [("interpreter", Cat::Cpu), ("gil", Cat::Lock), ("gc", Cat::Cpu)]);
        assert!((h[0].share - 0.6).abs() < 1e-9);
        assert!((h[1].share - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_c_program_with_python_like_names_is_not_python() {
        let (lang, h) = found(&[(Cat::Cpu, 1.0, &[("gc_collect", "mylisp"), ("take_gil", "mylisp"), ("main", "mylisp")])]);
        assert_eq!(lang, None);
        assert!(h.is_empty());
    }

    #[test]
    fn native_allocation_by_language() {
        // Rust: the same malloc, Rust's advice.
        let (lang, h) = found(&[
            (Cat::Cpu, 3.0, &[("_int_malloc", "libc.so.6"), ("malloc", "libc.so.6"), ("alloc::raw_vec::RawVec<T,A>::grow_one", "app"), ("app::parse", "app"), ("std::rt::lang_start", "app")]),
            (Cat::Cpu, 1.0, &[("<core::hash::sip::Hasher<S> as core::hash::Hasher>::write", "app"), ("app::index", "app"), ("std::rt::lang_start", "app")]),
            (Cat::Cpu, 6.0, &[("app::work", "app"), ("std::rt::lang_start", "app")]),
        ]);
        assert_eq!(lang, Some(Lang::Rust));
        assert_eq!((h[0].id, h[0].lang), ("alloc", Some(Lang::Rust)));
        assert!(h[0].fix.contains("with_capacity"));
        assert_eq!(h[1].id, "siphash");
        // Dropping a map is not hashing, whatever hasher its type names.
        let (_, h) = found(&[(Cat::Cpu, 1.0, &[
            ("core::ptr::drop_in_place<std::collections::hash::map::HashMap<u32,u32,std::hash::random::RandomState>>", "app"),
            ("app::work", "app"),
            ("std::rt::lang_start", "app"),
        ])]);
        assert!(h.is_empty(), "{h:?}");
        // C++: operator new, and no hint for a function that only takes a stream.
        let (lang, h) = found(&[
            (Cat::Cpu, 3.0, &[("malloc", "libc.so.6"), ("operator new(unsigned long)", "libstdc++.so.6"), ("Parser::next(int)", "app"), ("main", "app")]),
            (Cat::Cpu, 7.0, &[("Report::print(std::basic_ostream<char, std::char_traits<char> >&) const", "app"), ("main", "app")]),
        ]);
        assert_eq!(lang, Some(Lang::C));
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].id, h[0].lang), ("alloc", Some(Lang::C)));
        assert!(h[0].fix.contains("LD_PRELOAD"));
        // Inside the JVM, malloc is the JVM's business.
        let (lang, h) = found(&[(Cat::Cpu, 1.0, &[("malloc", "libc.so.6"), ("os::malloc(unsigned long, MEMFLAGS)", "libjvm.so"), ("thread_native_entry(Thread*)", "libjvm.so")])]);
        assert_eq!(lang, Some(Lang::Jvm));
        assert!(h.is_empty());
    }

    #[test]
    fn what_a_function_calls_back_into_is_not_its_time() {
        // A test runner calls each test through reflection: the test's
        // time is the test's. Looking the method up is reflection's.
        let jit = "[jit]";
        let (lang, h) = found(&[
            (Cat::Cpu, 8.0, &[
                ("int com.shop.PriceTest.sum(int)", jit),
                ("java.lang.Object jdk.internal.reflect.DirectMethodHandleAccessor.invoke(java.lang.Object, java.lang.Object[])", jit),
                ("java.lang.Object java.lang.reflect.Method.invoke(java.lang.Object, java.lang.Object[])", jit),
                ("void org.junit.Runner.run()", jit),
            ]),
            (Cat::Cpu, 2.0, &[
                ("java.lang.reflect.Method java.lang.Class.getMethod0(java.lang.String, java.lang.Class[])", jit),
                ("java.lang.reflect.Method java.lang.Class.getMethod(java.lang.String, java.lang.Class[])", jit),
                ("void org.junit.Runner.run()", jit),
            ]),
        ]);
        assert_eq!(lang, Some(Lang::Jvm));
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].id, "reflection");
        assert!((h[0].share - 0.2).abs() < 1e-9);
        assert_eq!(h[0].seen, "java.lang.Class.getMethod0");
        // printf under an ostream is the ostream's.
        let (_, h) = found(&[
            (Cat::Cpu, 1.0, &[
                ("__vfprintf", "libsystem_c.dylib"),
                ("snprintf_l", "libsystem_c.dylib"),
                ("std::__1::num_put<char, std::__1::ostreambuf_iterator<char> >::do_put(std::__1::ostreambuf_iterator<char>, std::__1::ios_base&, char, double) const", "libc++.1.dylib"),
                ("main", "app"),
            ]),
            (Cat::Cpu, 1.0, &[("__vfprintf", "libsystem_c.dylib"), ("printf", "libsystem_c.dylib"), ("report(int)", "app"), ("main", "app")]),
        ]);
        let ids: Vec<(&str, f64)> = h.iter().map(|x| (x.id, x.share)).collect();
        assert_eq!(ids, [("iostream", 0.5), ("stdio", 0.5)]);
        // A script runs under node's loader: that is not start-up.
        let node = "node";
        let (lang, h) = found(&[
            (Cat::Cpu, 9.0, &[
                ("v8::internal::JsonStringifier::Serialize_<true>(v8::internal::Handle<v8::internal::Object>, bool, v8::internal::Handle<v8::internal::Object>)", node),
                ("Builtins_JsonStringify", node),
                ("node::builtins::BuiltinLoader::CompileAndCall(v8::Local<v8::Context>, char const*, node::Realm*)", node),
                ("node::Start(int, char**)", node),
            ]),
            (Cat::Cpu, 1.0, &[
                ("v8::internal::Parser::ParseProgram(v8::internal::Isolate*, v8::internal::Handle<v8::internal::Script>, v8::internal::ParseInfo*)", node),
                ("v8::internal::Compiler::GetSharedFunctionInfoForScript(v8::internal::Isolate*)", node),
                ("node::builtins::BuiltinLoader::CompileAndCall(v8::Local<v8::Context>, char const*, node::Realm*)", node),
                ("node::Start(int, char**)", node),
            ]),
        ]);
        assert_eq!(lang, Some(Lang::Node));
        let ids: Vec<(&str, f64)> = h.iter().map(|x| (x.id, x.share)).collect();
        assert_eq!(ids, [("json", 0.9), ("startup", 0.1)]);
    }

    #[test]
    fn signs_about_the_build_come_first() {
        let (_, h) = found(&[
            (Cat::Cpu, 0.05, &[("core::ptr::copy_nonoverlapping::precondition_check", "app"), ("app::work", "app"), ("std::rt::lang_start", "app")]),
            (Cat::Cpu, 5.0, &[("malloc", "libc.so.6"), ("alloc::alloc::exchange_malloc", "app"), ("app::work", "app"), ("std::rt::lang_start", "app")]),
            (Cat::Cpu, 5.0, &[("app::work", "app"), ("std::rt::lang_start", "app")]),
        ]);
        assert_eq!(h[0].id, "debug-build");
        assert!(h[0].whole);
        assert_eq!(h[1].id, "alloc");
    }

    #[test]
    fn small_causes_and_small_shares_say_nothing() {
        // 3% of the CPU time: below what is worth a hint.
        let (_, h) = found(&[
            (Cat::Cpu, 0.3, &[("regexp.(*Regexp).FindString", "srv"), ("main.a", "srv"), ("runtime.goexit", "srv")]),
            (Cat::Cpu, 9.7, &[("main.b", "srv"), ("runtime.goexit", "srv")]),
        ]);
        assert!(h.is_empty());
        // All of the lock time, but lock is 2% of the whole.
        let (_, h) = found(&[
            (Cat::Lock, 0.2, &[("sync.(*Mutex).Lock", "srv"), ("main.a", "srv"), ("runtime.goexit", "srv")]),
            (Cat::Cpu, 9.8, &[("main.b", "srv"), ("runtime.goexit", "srv")]),
        ]);
        assert!(h.is_empty());
    }
}
