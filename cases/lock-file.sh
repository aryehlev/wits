#!/usr/bin/env bash
# A process queued on a file lock that another process holds.
# expect: "cause":"lock"
# expect: "kind":"process"
# expect: wits-lock-case
# expect: sleep 300
. "$(dirname "$0")/lib.sh"
command -v flock >/dev/null || skip "needs flock"

flock "$WORK/wits-lock-case" sleep 300 &
settle
flock "$WORK/wits-lock-case" true &
settle
explain --no-follow $!
