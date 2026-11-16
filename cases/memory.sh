#!/usr/bin/env bash
# A process that needs more memory than its cgroup allows, so the kernel keeps
# evicting and re-reading its pages.
# expect: "cause":"memory"
# expect: "kind":"cgroup"
. "$(dirname "$0")/lib.sh"
command -v python3 >/dev/null || skip "needs python3"

CG=
if [ -w /sys/fs/cgroup/cgroup.subtree_control ] && grep -qw memory /sys/fs/cgroup/cgroup.controllers; then
  CG=/sys/fs/cgroup/wits-case-$$                       # cgroup v2
  mkdir "$CG" 2>/dev/null || skip "cannot create a cgroup"
  echo +memory > /sys/fs/cgroup/cgroup.subtree_control 2>/dev/null
  echo $((48 << 20)) > "$CG/memory.high" 2>/dev/null || { rmdir "$CG"; skip "cannot set memory.high"; }
elif [ -w /sys/fs/cgroup/memory ]; then
  CG=/sys/fs/cgroup/memory/wits-case-$$                # cgroup v1
  mkdir "$CG" 2>/dev/null || skip "cannot create a cgroup"
  echo $((48 << 20)) > "$CG/memory.limit_in_bytes" 2>/dev/null || { rmdir "$CG"; skip "cannot set a memory limit"; }
else
  skip "needs a writable memory cgroup (run as root)"
fi

dd if=/dev/zero of="$WORK/big" bs=1M count=256 conv=fsync 2>/dev/null
python3 - "$CG" "$WORK/big" <<'PY' &
import mmap, os, random, sys
open(sys.argv[1] + "/cgroup.procs", "w").write(str(os.getpid()))
f = open(sys.argv[2], "r+b")
# Drop the file from the page cache so that reading it is charged to us.
os.posix_fadvise(f.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)
m = mmap.mmap(f.fileno(), 0)
n = len(m) // 4096
while True:
    m[random.randrange(n) * 4096]
PY
PID=$!
settle 2
explain $PID
kill $PID; wait $PID 2>/dev/null
rmdir "$CG" 2>/dev/null
