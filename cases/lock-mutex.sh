#!/usr/bin/env bash
# A thread queued on a mutex held by another thread of the same process.
# expect: "cause":"lock"
# expect: "kind":"thread"
# expect: "name":"holder"
. "$(dirname "$0")/lib.sh"
command -v cc >/dev/null || skip "needs a C compiler"

cc -O1 -pthread -o "$WORK/lock-mutex" "$HERE/lock-mutex.c" || skip "could not compile"
"$WORK/lock-mutex" &
settle 1.5
explain $!
