# Starts a handful of slow processes with recognisable names, for the demo
# recording and for trying wits out. Source it, then ask wits about them:
#
#   . demo/setup.sh          # start        (optional argument: directory to use)
#   wits                     # what is waiting right now?
#   wits deploy              # why is deploy slow?
#   demo_stop                # clean up
#
# Every "program" here is a copy of a coreutils binary under another name.

DEMO_DIR=${1:-$(mktemp -d "${TMPDIR:-/tmp}/wits-demo.XXXXXX")}
mkdir -p "$DEMO_DIR"
_demo_as() { cp "$(command -v "$1")" "$DEMO_DIR/$2"; }
_demo_as dd backup
_demo_as flock migrate
_demo_as flock deploy
_demo_as cat report
_demo_as sleep warehouse
_demo_as yes encoder
_demo_as sh api

DEMO_PIDS=()
# backup: every write waits for the disk
"$DEMO_DIR/backup" if=/dev/zero of="$DEMO_DIR/backup.img" bs=4k count=100000000 oflag=dsync 2>/dev/null &
DEMO_PIDS+=($!)
# migrate holds a lock that deploy needs
"$DEMO_DIR/migrate" "$DEMO_DIR/schema.lock" "$DEMO_DIR/warehouse" 600 &
DEMO_PIDS+=($!)
sleep 0.3
"$DEMO_DIR/deploy" "$DEMO_DIR/schema.lock" true &
DEMO_PIDS+=($!)
# report reads from a warehouse that sends nothing
mkfifo "$DEMO_DIR/rows"
"$DEMO_DIR/warehouse" 600 > "$DEMO_DIR/rows" &
DEMO_PIDS+=($!)
"$DEMO_DIR/report" < "$DEMO_DIR/rows" > /dev/null &
DEMO_PIDS+=($!)
# api shares one CPU with two encoders
if command -v taskset > /dev/null; then
  for _ in 1 2; do
    taskset -c 0 "$DEMO_DIR/encoder" > /dev/null &
    DEMO_PIDS+=($!)
  done
  taskset -c 0 "$DEMO_DIR/api" -c 'while :; do :; done' &
  DEMO_PIDS+=($!)
fi

demo_stop() {
  kill "${DEMO_PIDS[@]}" 2> /dev/null
  pkill -f "^$DEMO_DIR/" 2> /dev/null
  rm -rf "$DEMO_DIR"
  unset DEMO_PIDS DEMO_DIR
}
