#!/usr/bin/env bash
# Run every case and check wits gives the expected answer.
#   cases/run.sh [path/to/wits] [case ...]
WITS=${1:-${WITS:-$(dirname "$0")/../target/release/wits}}
[ $# -gt 0 ] && shift
WITS=$(realpath "$WITS") || exit 1
cd "$(dirname "$0")" || exit 1
[ -x "$WITS" ] || { echo "no wits binary at $WITS (cargo build --release first)"; exit 1; }
CASES=("$@")
[ ${#CASES[@]} -eq 0 ] && CASES=(cpu cpu-wait cpu-limit disk memory lock-file lock-mutex peer-pipe peer-tcp child idle
  where-cpu where-disk run-diff pod-netns agent-follow)

# With kernel.yama.ptrace_scope >= 1 (the Ubuntu default) an ordinary user
# cannot read the system call of a process that is not its descendant, so
# cases that need it are skipped. Run as root to cover them.
limited=0
if [ "$(id -u)" != 0 ] && [ "$(cat /proc/sys/kernel/yama/ptrace_scope 2>/dev/null || echo 0)" != 0 ]; then
  limited=1
  echo "note: ptrace_scope restricts this user; cases marked 'needs: syscall' are skipped"
fi

pass=0 fail=0 skipped=0
for c in "${CASES[@]}"; do
  f=${c%.sh}.sh
  if [ "$(id -u)" != 0 ] && grep -q '^# needs: root' "$f"; then
    printf 'skip  %s (needs root)\n' "$c"; skipped=$((skipped + 1)); continue
  fi
  if [ $limited -eq 1 ] && grep -q '^# needs: syscall' "$f"; then
    printf 'skip  %s\n' "$c"; skipped=$((skipped + 1)); continue
  fi
  out=$(WITS=$WITS WITS_ARGS=--json bash "$f" 2>/dev/null </dev/null)
  rc=$?
  if [ $rc -eq 77 ]; then
    printf 'skip  %s\n' "$c"; skipped=$((skipped + 1)); continue
  fi
  ok=1
  while IFS= read -r want; do
    grep -qF -- "$want" <<<"$out" || { ok=0; printf '      missing: %s\n' "$want"; }
  done < <(sed -n 's/^# expect: //p' "$f")
  if [ $ok -eq 1 ]; then
    printf 'ok    %s\n' "$c"; pass=$((pass + 1))
  else
    printf 'FAIL  %s\n      got: %s\n' "$c" "$out"; fail=$((fail + 1))
  fi
done
echo "$pass passed, $fail failed, $skipped skipped"
[ $fail -eq 0 ]
