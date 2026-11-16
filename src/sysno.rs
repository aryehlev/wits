//! Maps system call numbers to the handful of classes wits cares about.
//! Numbers come from the libc crate, so they are right for the build target.

use libc::c_long;

// Some kinds only occur in the system call table of one architecture.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Reads from fd in arg 0.
    Read,
    /// Writes to fd in arg 0.
    Write,
    /// splice(in, _, out, ..): moves data between two descriptors.
    Splice,
    /// sendfile(out, in, ..)
    Sendfile,
    /// Flushes fd in arg 0 to storage.
    Sync,
    /// Flushes with no fd (sync).
    SyncAll,
    /// Path or metadata operation; only blocks on storage.
    FileMeta,
    /// open(path, ..): path pointer in arg 0.
    Open0,
    /// openat(dirfd, path, ..): path pointer in arg 1.
    Open1,
    Connect,
    Accept,
    /// poll(fds, nfds, ..)
    Poll,
    /// select(nfds, readfds, ..)
    Select,
    /// epoll_wait(epfd, ..)
    Epoll,
    Futex,
    Flock,
    Fcntl,
    Wait,
    Sleep,
    SigWait,
    Uring,
    Ipc,
    Fork,
    Other,
}

macro_rules! table {
    ($( $name:ident => $kind:ident ),* $(,)?) => {
        &[ $( (libc::$name, Kind::$kind, {
            // "SYS_read" -> "read"
            const N: &str = stringify!($name);
            N
        }) ),* ]
    };
}

const COMMON: &[(c_long, Kind, &str)] = table! {
    SYS_read => Read, SYS_readv => Read, SYS_pread64 => Read, SYS_preadv => Read,
    SYS_preadv2 => Read, SYS_recvfrom => Read, SYS_recvmsg => Read, SYS_recvmmsg => Read,
    SYS_write => Write, SYS_writev => Write, SYS_pwrite64 => Write, SYS_pwritev => Write,
    SYS_pwritev2 => Write, SYS_sendto => Write, SYS_sendmsg => Write, SYS_sendmmsg => Write,
    SYS_sendfile => Sendfile, SYS_splice => Splice, SYS_copy_file_range => Splice,
    SYS_fsync => Sync, SYS_fdatasync => Sync, SYS_syncfs => Sync, SYS_sync_file_range => Sync,
    SYS_sync => SyncAll, SYS_msync => SyncAll,
    SYS_openat => Open1,
    SYS_close => FileMeta, SYS_getdents64 => FileMeta, SYS_newfstatat => FileMeta,
    SYS_statx => FileMeta, SYS_fstat => FileMeta, SYS_unlinkat => FileMeta,
    SYS_renameat2 => FileMeta, SYS_mkdirat => FileMeta, SYS_ftruncate => FileMeta,
    SYS_truncate => FileMeta, SYS_fallocate => FileMeta, SYS_readlinkat => FileMeta,
    SYS_faccessat => FileMeta, SYS_fadvise64 => FileMeta, SYS_readahead => FileMeta,
    SYS_execve => FileMeta, SYS_mmap => FileMeta, SYS_munmap => FileMeta,
    SYS_connect => Connect,
    SYS_accept => Accept, SYS_accept4 => Accept,
    SYS_ppoll => Poll,
    SYS_pselect6 => Select,
    SYS_epoll_pwait => Epoll, SYS_epoll_pwait2 => Epoll,
    SYS_futex => Futex,
    SYS_flock => Flock,
    SYS_fcntl => Fcntl,
    SYS_wait4 => Wait, SYS_waitid => Wait,
    SYS_nanosleep => Sleep, SYS_clock_nanosleep => Sleep,
    SYS_rt_sigsuspend => SigWait, SYS_rt_sigtimedwait => SigWait,
    SYS_io_uring_enter => Uring, SYS_io_getevents => Uring,
    SYS_msgrcv => Ipc, SYS_msgsnd => Ipc, SYS_semop => Ipc, SYS_semtimedop => Ipc,
    SYS_mq_timedreceive => Ipc, SYS_mq_timedsend => Ipc,
    SYS_clone => Fork, SYS_clone3 => Fork,
};

#[cfg(target_arch = "x86_64")]
const ARCH: &[(c_long, Kind, &str)] = table! {
    SYS_open => Open0, SYS_creat => Open0,
    SYS_stat => FileMeta, SYS_lstat => FileMeta, SYS_getdents => FileMeta,
    SYS_unlink => FileMeta, SYS_rename => FileMeta, SYS_mkdir => FileMeta,
    SYS_rmdir => FileMeta, SYS_access => FileMeta, SYS_readlink => FileMeta,
    SYS_renameat => FileMeta,
    SYS_poll => Poll,
    SYS_select => Select,
    SYS_epoll_wait => Epoll,
    SYS_pause => SigWait,
    SYS_fork => Fork, SYS_vfork => Fork,
};

#[cfg(not(target_arch = "x86_64"))]
const ARCH: &[(c_long, Kind, &str)] = &[];

pub fn lookup(nr: i64) -> (Kind, &'static str) {
    for t in [COMMON, ARCH] {
        for &(n, k, name) in t {
            if n as i64 == nr {
                return (k, name.strip_prefix("SYS_").unwrap_or(name));
            }
        }
    }
    (Kind::Other, "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_calls() {
        assert_eq!(lookup(libc::SYS_read as i64), (Kind::Read, "read"));
        assert_eq!(lookup(libc::SYS_futex as i64), (Kind::Futex, "futex"));
        assert_eq!(lookup(libc::SYS_fsync as i64).0, Kind::Sync);
        assert_eq!(lookup(999_999).0, Kind::Other);
    }

    #[test]
    fn no_duplicate_numbers() {
        let mut seen = std::collections::HashSet::new();
        for t in [COMMON, ARCH] {
            for &(n, _, name) in t {
                assert!(seen.insert(n), "duplicate syscall number for {name}");
            }
        }
    }
}
