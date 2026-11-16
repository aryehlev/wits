#!/usr/bin/env bash
# Two fake nodes (separate network and PID namespaces). node-b runs a server
# that fsyncs on every request, and a wits agent. wits on node-a explains a
# client of that server: it must ask the agent, and show what the server
# itself is waiting for.
# needs: root
# expect: "node":"node-b"
# expect: "upstream":
# expect: "cause":"disk"
# expect: fsync
. "$(dirname "$0")/lib.sh"
command -v python3 >/dev/null && command -v ip >/dev/null && command -v unshare >/dev/null || skip "needs python3, ip and unshare"
cleanup() { ip netns del wits-na 2>/dev/null; ip netns del wits-nb 2>/dev/null; return 0; }
cleanup
ip netns add wits-na && ip netns add wits-nb || skip "cannot create network namespaces"
ip link add wits-vna type veth peer name wits-vnb
ip link set wits-vna netns wits-na; ip link set wits-vnb netns wits-nb
ip -n wits-na addr add 10.233.0.1/24 dev wits-vna; ip -n wits-na link set wits-vna up
ip -n wits-nb addr add 10.233.0.2/24 dev wits-vnb; ip -n wits-nb link set wits-vnb up
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
import socket
c = socket.create_connection(("10.233.0.2", 5432))
while True:
    c.sendall(b"q"); c.recv(100)
PY
export WITS_TOKEN=case-token
# The agent is pid 1 of its PID namespace, and `unshare --fork` ignores
# SIGTERM: SIGKILL unshare, and --kill-child takes the namespace down with it.
# The agent must not hold our stdout.
NODE_NAME=node-b ip netns exec wits-nb unshare --pid --fork --kill-child --mount-proc \
  bash -c "python3 $WORK/server.py $WORK/srv.log & exec $WITS agent --listen 10.233.0.2:7477" >/dev/null 2>&1 &
agent=$!
settle 1
NODE_NAME=node-a ip netns exec wits-na unshare --pid --fork --kill-child --mount-proc \
  bash -c "python3 $WORK/client.py & sleep 1; $WITS ${WITS_ARGS:-} -t ${T:-2} --no-stacks --peers 10.233.0.2 python3"
kill -KILL "$agent"; wait "$agent" 2>/dev/null
cleanup
