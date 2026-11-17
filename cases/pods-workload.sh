#!/usr/bin/env bash
# Three replicas of a Deployment called web, on one node: one stuck on
# fsync (with an idle sidecar beside it), one computing, one idle; and a
# Deployment called web-gateway that must not count as web. Fake pods:
# kubepods cgroups and the kubelet's log layout. wits must explain every
# replica, name the stuck one, and leave web-gateway out; asked about the
# stuck pod alone, it must explain both of its containers and pick the
# application, not the sidecar.
# needs: root
# expect: "kind":"group"
# expect: "pod":"shop/web-7d9f8b6c5-x2k4q/app"
# expect: "pod":"shop/web-7d9f8b6c5-b5n8w/app"
# expect: "pod":"shop/web-7d9f8b6c5-zz9rt/app"
# expect: "pod":"shop/web-7d9f8b6c5-x2k4q/istio-proxy"
# expect: pod shop/web-7d9f8b6c5-x2k4q/app) is waiting for disk
# reject: web-gateway
. "$(dirname "$0")/lib.sh"
command -v python3 >/dev/null || skip "needs python3"
[ "$(stat -fc %T /sys/fs/cgroup)" = cgroup2fs ] || skip "needs cgroup v2"
H=$WORK/host
POD=/sys/fs/cgroup/kubepods.slice/kubepods-burstable.slice
n=0
# pod name, container, command...: a fake container running the command.
start() {
  local pod=$1 c=$2; shift 2
  n=$((n + 1))
  local uid cid
  uid=$(printf '%08d-1111-2222-3333-%012d' "$n" "$n")
  # One uid per pod: reuse it for a second container.
  local seen; seen=$(ls -d "$H"/var/log/pods/shop_"$pod"_* 2>/dev/null | head -1)
  [ -n "$seen" ] && uid=${seen##*_}
  cid=$(printf '%064d' "$n")
  mkdir -p "$H/var/log/pods/shop_${pod}_$uid/$c" "$H/var/log/containers"
  touch "$H/var/log/containers/${pod}_shop_$c-$cid.log"
  local d="$POD/kubepods-burstable-pod${uid//-/_}.slice/cri-containerd-$cid.scope"
  mkdir -p "$d"
  bash -c "echo \$\$ > $d/cgroup.procs; exec $*" &
}
cleanup() {
  for d in "$POD"/kubepods-burstable-pod*_1111_2222_3333_*.slice; do
    [ -d "$d" ] || continue
    for s in "$d"/cri-containerd-*.scope; do
      for p in $(cat "$s/cgroup.procs" 2>/dev/null); do kill -9 "$p" 2>/dev/null; done
    done
  done
  sleep 0.3
  for d in "$POD"/kubepods-burstable-pod*_1111_2222_3333_*.slice; do
    [ -d "$d" ] && rmdir "$d"/cri-containerd-*.scope "$d" 2>/dev/null
  done
  rmdir "$POD" /sys/fs/cgroup/kubepods.slice 2>/dev/null
  return 0
}
cleanup >/dev/null 2>&1
mkdir -p "$POD" || skip "cannot create cgroups"
cat > "$WORK/fsync.py" <<'PY'
import os, sys
f = open(sys.argv[1], "ab")
while True:
    f.write(b"x" * 4096); f.flush(); os.fsync(f.fileno())
PY
start web-7d9f8b6c5-x2k4q app python3 "$WORK/fsync.py" "$WORK/log.bin"
start web-7d9f8b6c5-x2k4q istio-proxy sleep 1000
start web-7d9f8b6c5-b5n8w app sh -c "'while :; do :; done'"
start web-7d9f8b6c5-zz9rt app sleep 1000
start web-gateway-6c8d9f7b5-q9w2x app sh -c "'while :; do :; done'"
settle 1
WITS_HOST_ROOT=$H explain --no-stacks shop/web
WITS_HOST_ROOT=$H explain --no-stacks shop/web-7d9f8b6c5-x2k4q
cleanup
