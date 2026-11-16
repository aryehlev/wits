#!/usr/bin/env bash
# Not every quiet process is slow: this one is asleep by choice.
# expect: "state":"idle"
# expect: "cause":null
# expect: asleep on its own timer
. "$(dirname "$0")/lib.sh"

sleep 300 &
settle
explain $!
