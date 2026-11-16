#!/usr/bin/env bash
# A process that waits for every write to reach the disk.
# expect: "cause":"disk"
# expect: "state":"stalled"
# expect: wits-disk-case
. "$(dirname "$0")/lib.sh"

dd if=/dev/zero of="$WORK/wits-disk-case" bs=4k count=10000000 oflag=dsync 2>/dev/null &
settle
explain $!
