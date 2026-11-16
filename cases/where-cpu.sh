#!/usr/bin/env bash
# A busy loop: eBPF on-CPU stacks name the function and line.
# needs: root
# expect: "cause":"cpu"
# expect: "where":"
# expect: inner_loop
# expect: where.c:
. "$(dirname "$0")/lib.sh"
command -v cc >/dev/null || skip "needs a C compiler"
cc -O1 -g -fno-omit-frame-pointer -o "$WORK/where" "$HERE/where.c" || skip "could not compile"
"$WORK/where" cpu &
settle 0.5
explain $!
