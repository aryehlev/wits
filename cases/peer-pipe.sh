#!/usr/bin/env bash
# A process reading a pipe whose writer has nothing to say.
# expect: "cause":"peer"
# expect: "state":"waiting"
# expect: "kind":"process"
# expect: sleep 300
. "$(dirname "$0")/lib.sh"

mkfifo "$WORK/p"
sleep 300 > "$WORK/p" &
cat < "$WORK/p" > /dev/null &
settle
explain $!
