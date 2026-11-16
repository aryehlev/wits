# Shared by every case. A case starts one slow condition, asks wits about it,
# prints the answer, and cleans up after itself.
#
#   WITS=path/to/wits   binary to test (default: wits on PATH)
#   WITS_ARGS=--json    extra arguments (run.sh uses this)
#   T=2                 seconds to watch

# Run in our own process group so cleanup can kill everything we started.
if [ "$(ps -o pgid= -p $$ | tr -d ' ')" != "$$" ]; then
  exec setsid -w bash "$0" "$@"
fi

WITS=${WITS:-wits}
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=$HERE/.work
mkdir -p "$WORK"
trap 'rc=$?; trap "" TERM; trap - EXIT; rm -rf "$WORK"; kill 0; wait 2>/dev/null; exit $rc' EXIT

explain() { "$WITS" ${WITS_ARGS:-} -t "${T:-2}" "$@"; }
skip() { echo "skip: $*" >&2; exit 77; }
settle() { sleep "${1:-0.5}"; }
