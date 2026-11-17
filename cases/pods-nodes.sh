#!/usr/bin/env bash
# The replicas of a Deployment on two fake nodes (separate network and PID
# namespaces, each with its own kubelet log layout). node-b runs a wits
# agent. Asked on node-a about the Deployment, wits must explain the
# replicas on both nodes; asked about the pod on node-b, it must get that
# pod's answer from node-b's agent.
# needs: root
# expect: "kind":"group"
# expect: "node":"node-a"
# expect: "node":"node-b"
# expect: "pod":"shop/web-7d9f8b6c5-b5n8w/app"
# expect: "pod":"shop/web-7d9f8b6c5-x2k4q/app"
# expect: "cause":"disk"
. "$(dirname "$0")/lib.sh"
command -v python3 >/dev/null && command -v ip >/dev/null && command -v unshare >/dev/null || skip "needs python3, ip and unshare"
[ "$(stat -fc %T /sys/fs/cgroup)" = cgroup2fs ] || skip "needs cgroup v2"
POD=/sys/fs/cgroup/kubepods.slice/kubepods-burstable.slice
UA=00000001-1111-2222-3333-000000000001 CA=$(printf 'c%.0s' {1..64})
UB=00000002-1111-2222-3333-000000000002 CB=$(printf 'd%.0s' {1..64})
cg() { echo "$POD/kubepods-burstable-pod${1//-/_}.slice/cri-containerd-$2.scope"; }
# node dir, pod name, uid, container id: the kubelet's log layout.
layout() {
  mkdir -p "$1/var/log/pods/shop_$2_$3/app" "$1/var/log/containers"
  touch "$1/var/log/containers/$2_shop_app-$4.log"
}
layout "$WORK/ha" web-7d9f8b6c5-b5n8w $UA $CA
layout "$WORK/hb" web-7d9f8b6c5-x2k4q $UB $CB
cleanup() {
  ip netns del wits-na 2>/dev/null; ip netns del wits-nb 2>/dev/null
  for d in "$(cg $UA $CA)" "$(cg $UB $CB)"; do
    for p in $(cat "$d/cgroup.procs" 2>/dev/null); do kill -9 "$p" 2>/dev/null; done
  done
  sleep 0.3
  rmdir "$(cg $UA $CA)" "$(cg $UB $CB)" "$(dirname "$(cg $UA $CA)")" "$(dirname "$(cg $UB $CB)")" \
    "$POD" /sys/fs/cgroup/kubepods.slice 2>/dev/null
  return 0
}
cleanup
mkdir -p "$(cg $UA $CA)" "$(cg $UB $CB)" || skip "cannot create cgroups"
ip netns add wits-na && ip netns add wits-nb || skip "cannot create network namespaces"
ip link add wits-vna type veth peer name wits-vnb
ip link set wits-vna netns wits-na; ip link set wits-vnb netns wits-nb
ip -n wits-na addr add 10.233.0.1/24 dev wits-vna; ip -n wits-na link set wits-vna up
ip -n wits-nb addr add 10.233.0.2/24 dev wits-vnb; ip -n wits-nb link set wits-vnb up
cat > "$WORK/fsync.py" <<'PY'
import os, sys
f = open(sys.argv[1], "ab")
while True:
    f.write(b"x" * 4096); f.flush(); os.fsync(f.fileno())
PY
export WITS_TOKEN=case-token
# Inside a node, cgroup2 is mounted again (`ip netns exec` remounted /sys)
# so the replica can join its pod's cgroup and the agent stays outside it.
# See agent-follow.sh for why the agent is killed with SIGKILL.
NODE_NAME=node-b WITS_HOST_ROOT=$WORK/hb ip netns exec wits-nb unshare --pid --fork --kill-child --mount-proc \
  bash -c "mount -t cgroup2 none /sys/fs/cgroup
    bash -c 'echo \$\$ > $(cg $UB $CB)/cgroup.procs; exec python3 $WORK/fsync.py $WORK/log.bin' &
    exec $WITS agent --listen 10.233.0.2:7477" >/dev/null 2>&1 &
agent=$!
NODE_NAME=node-a WITS_HOST_ROOT=$WORK/ha ip netns exec wits-na unshare --pid --fork --kill-child --mount-proc \
  bash -c "mount -t cgroup2 none /sys/fs/cgroup
    bash -c 'echo \$\$ > $(cg $UA $CA)/cgroup.procs; exec sh -c \"while :; do :; done\"' &
    sleep 1.5
    $WITS ${WITS_ARGS:-} -t ${T:-2} --no-stacks --peers 10.233.0.2 shop/web
    $WITS ${WITS_ARGS:-} -t ${T:-2} --no-stacks --peers 10.233.0.2 shop/web-7d9f8b6c5-x2k4q"
kill -KILL "$agent"; wait "$agent" 2>/dev/null
cleanup
