#!/usr/bin/env bash
# fsync in a loop: the stack of the blocked thread names the caller.
# needs: syscall
# expect: "cause":"disk"
# expect: flush_log
# expect: where.c:
. "$(dirname "$0")/lib.sh"
command -v cc >/dev/null || skip "needs a C compiler"
cc -O1 -g -fno-omit-frame-pointer -o "$WORK/where" "$HERE/where.c" || skip "could not compile"
"$WORK/where" disk "$WORK/log.bin" &
settle 0.5
explain $!
