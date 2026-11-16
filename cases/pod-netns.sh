#!/usr/bin/env bash
# Two fake pods on one node, each in its own network namespace and kubepods
# cgroup, with the kubelet's log layout. wits must name both pods and find
# the server across namespaces, also through a Service-style DNAT address.
# needs: root
# expect: "pod":"shop/api-7d9f/api"
# expect: "pod":"db/pg-0/postgres"
# expect: "cause":"peer"
# expect: "kind":"process"
. "$(dirname "$0")/lib.sh"
command -v python3 >/dev/null && command -v ip >/dev/null || skip "needs python3 and ip"
[ "$(stat -fc %T /sys/fs/cgroup)" = cgroup2fs ] || skip "needs cgroup v2"
command -v iptables >/dev/null || skip "needs iptables"
UA=11111111-2222-3333-4444-555555555555 CA=$(printf 'a%.0s' {1..64})
UB=66666666-7777-8888-9999-000000000000 CB=$(printf 'b%.0s' {1..64})
cg() { echo "/sys/fs/cgroup/kubepods.slice/kubepods-burstable.slice/kubepods-burstable-pod${1//-/_}.slice/cri-containerd-$2.scope"; }
H=$WORK/host
mkdir -p "$H/var/log/pods/shop_api-7d9f_$UA/api" "$H/var/log/pods/db_pg-0_$UB/postgres" "$H/var/log/containers"
touch "$H/var/log/containers/api-7d9f_shop_api-$CA.log" "$H/var/log/containers/pg-0_db_postgres-$CB.log"
mkdir -p "$(cg $UA $CA)" "$(cg $UB $CB)"
cleanup() {
  ip netns del wits-pa 2>/dev/null; ip netns del wits-pb 2>/dev/null
  for d in "$(cg $UA $CA)" "$(cg $UB $CB)"; do
    for p in $(cat "$d/cgroup.procs" 2>/dev/null); do kill -9 "$p" 2>/dev/null; done
  done
  sleep 0.3
  rmdir "$(cg $UA $CA)" "$(cg $UB $CB)" "$(dirname "$(cg $UA $CA)")" "$(dirname "$(cg $UB $CB)")" \
    /sys/fs/cgroup/kubepods.slice/kubepods-burstable.slice /sys/fs/cgroup/kubepods.slice 2>/dev/null
  return 0
}
cleanup >/dev/null 2>&1; mkdir -p "$(cg $UA $CA)" "$(cg $UB $CB)"
ip netns add wits-pa && ip netns add wits-pb || skip "cannot create network namespaces"
ip link add wits-va type veth peer name wits-vb
ip link set wits-va netns wits-pa; ip link set wits-vb netns wits-pb
ip -n wits-pa addr add 10.231.0.1/24 dev wits-va; ip -n wits-pa link set wits-va up
ip -n wits-pb addr add 10.231.0.2/24 dev wits-vb; ip -n wits-pb link set wits-vb up
ip -n wits-pa route add 10.232.0.0/16 dev wits-va
ip netns exec wits-pa iptables -t nat -A OUTPUT -d 10.232.0.20 -p tcp --dport 5432 -j DNAT --to-destination 10.231.0.2:5432
cat > "$WORK/server.py" <<'PY'
import socket, os, sys
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("0.0.0.0", 5432)); s.listen()
f = open(sys.argv[1], "ab")
while True:
    c, _ = s.accept()
    while c.recv(100):
        for i in range(8):
            f.write(b"x" * 4096); f.flush(); os.fsync(f.fileno())
        c.sendall(b"ok")
PY
cat > "$WORK/client.py" <<'PY'
import socket, sys
c = socket.create_connection((sys.argv[1], 5432))
while True:
    c.sendall(b"q"); c.recv(100)
PY
# Join the cgroup before entering the namespace: `ip netns exec` remounts /sys.
bash -c "echo \$\$ > $(cg $UB $CB)/cgroup.procs; exec ip netns exec wits-pb python3 $WORK/server.py $WORK/srv.log" &
settle 0.5
bash -c "echo \$\$ > $(cg $UA $CA)/cgroup.procs; exec ip netns exec wits-pa python3 $WORK/client.py 10.232.0.20" &
settle 1
WITS_HOST_ROOT=$H explain --no-stacks shop/api-7d9f
WITS_HOST_ROOT=$H explain --no-stacks db/pg-0
cleanup
