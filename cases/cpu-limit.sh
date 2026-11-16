#!/usr/bin/env bash
# A process held back by its cgroup's CPU quota (what a container limit does).
# expect: "cause":"cpu-wait"
# expect: "kind":"cgroup"
. "$(dirname "$0")/lib.sh"

CG=
if [ -w /sys/fs/cgroup/cgroup.subtree_control ] && grep -qw cpu /sys/fs/cgroup/cgroup.controllers; then
  CG=/sys/fs/cgroup/wits-case-$$                       # cgroup v2
  mkdir "$CG" 2>/dev/null || skip "cannot create a cgroup"
  echo +cpu > /sys/fs/cgroup/cgroup.subtree_control 2>/dev/null
  echo "20000 100000" > "$CG/cpu.max" 2>/dev/null || { rmdir "$CG"; skip "cannot set cpu.max"; }
elif [ -w /sys/fs/cgroup/cpu ]; then
  CG=/sys/fs/cgroup/cpu/wits-case-$$                   # cgroup v1
  mkdir "$CG" 2>/dev/null || skip "cannot create a cgroup"
  echo 20000 > "$CG/cpu.cfs_quota_us" 2>/dev/null || { rmdir "$CG"; skip "cannot set a CPU quota"; }
else
  skip "needs a writable cpu cgroup (run as root)"
fi

sh -c 'echo $$ > "$0/cgroup.procs"; exec sh -c "while :; do :; done"' "$CG" &
PID=$!
settle 1
explain $PID
kill $PID; wait $PID 2>/dev/null
rmdir "$CG"
