#!/usr/bin/env bash
# A process that is simply computing. Nothing is in its way.
# expect: "cause":"cpu"
# expect: "state":"working"
# expect: "kind":"self"
. "$(dirname "$0")/lib.sh"

sh -c 'while :; do :; done' &
settle
explain $!
