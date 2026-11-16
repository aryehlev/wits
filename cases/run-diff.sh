#!/usr/bin/env bash
# The improvement loop: run a command, make it do a tenth of the work, run
# it again, and compare.
# expect: "kind":"diff"
# expect: "verdict":"faster"
. "$(dirname "$0")/lib.sh"
command -v cc >/dev/null || skip "needs a C compiler"
cc -O1 -g -fno-omit-frame-pointer -o "$WORK/where" "$HERE/where.c" || skip "could not compile"
"$WITS" run -n 3 -o "$WORK/before.json" -- "$WORK/where" 300 2>/dev/null || exit 1
"$WITS" run -n 3 -o "$WORK/after.json" -- "$WORK/where" 30 2>/dev/null || exit 1
"$WITS" diff --json "$WORK/before.json" "$WORK/after.json"
