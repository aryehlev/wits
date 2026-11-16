#!/usr/bin/env bash
# Asking about a parent that is only waiting for its child: wits follows the
# chain down and explains the child.
# expect: "cause":"cpu"
# expect: "followed":[{"pid":
. "$(dirname "$0")/lib.sh"

sh -c 'sh -c "while :; do :; done"; :' &
settle
explain $!
