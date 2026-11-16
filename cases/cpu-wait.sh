#!/usr/bin/env bash
# A process that wants to run but shares one CPU with three hogs.
# expect: "cause":"cpu-wait"
# expect: "kind":"process"
# expect: "name":"yes"
. "$(dirname "$0")/lib.sh"
command -v taskset >/dev/null || skip "needs taskset"

for _ in 1 2 3; do taskset -c 0 yes >/dev/null & done
taskset -c 0 sh -c 'while :; do :; done' &
settle
explain $!
