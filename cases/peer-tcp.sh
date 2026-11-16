#!/usr/bin/env bash
# A client that sent a request to a server which never answers.
# expect: "cause":"peer"
# expect: "kind":"process"
# expect: no reply since
# expect: slow-server
. "$(dirname "$0")/lib.sh"
command -v python3 >/dev/null || skip "needs python3"

cat > "$WORK/slow-server.py" <<'PY'
import socket, sys, time
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", 0))
s.listen()
print(s.getsockname()[1], flush=True)
c, _ = s.accept()
c.recv(100)
time.sleep(300)
PY
mkfifo "$WORK/port"
python3 "$WORK/slow-server.py" > "$WORK/port" &
read -r PORT < "$WORK/port"
python3 -c '
import socket, sys
c = socket.create_connection(("127.0.0.1", int(sys.argv[1])))
c.sendall(b"SELECT 1\n")
c.recv(100)' "$PORT" &
settle 1.3
explain $!
