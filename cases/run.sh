#!/usr/bin/env bash
# Run every case and check wits gives the expected answer.
#   cases/run.sh [path/to/wits] [case ...]
WITS=${1:-${WITS:-$(dirname "$0")/../target/release/wits}}
[ $# -gt 0 ] && shift
WITS=$(realpath "$WITS") || exit 1
cd "$(dirname "$0")" || exit 1
[ -x "$WITS" ] || { echo "no wits binary at $WITS (cargo build --release first)"; exit 1; }
CASES=("$@")
[ ${#CASES[@]} -eq 0 ] && CASES=(cpu cpu-wait cpu-limit disk memory lock-file lock-mutex peer-pipe peer-tcp child idle)

pass=0 fail=0 skipped=0
for c in "${CASES[@]}"; do
  f=${c%.sh}.sh
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
