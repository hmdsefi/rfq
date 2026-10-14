#!/usr/bin/env bash
# End-to-end proof of the RFQ engine on Aeron + Archive, in Docker:
#
#   1. builds the images and starts a fresh stack (media driver + Archive, then the engine);
#   2. runs every scenario through the demo client, which prints `PASS <scenario>` for each;
#   3. kills the engine with SIGKILL, restarts it, and checks that replaying the Archive journal
#      rebuilds exactly the state it had: the `replayed seq/hash` the engine logs after the restart
#      must equal the last `applied seq/hash` it logged before the kill. Prints `RECOVERY OK`.
#
# Any failure stops the script with a non-zero status and the engine's recent log. The stack is
# left running: re-run the scenarios against the same engine with `docker compose run --rm demo`,
# and stop everything with `docker compose --profile demo down -v`.
#
# RFQ_DEMO_TIMEOUT: seconds to wait for the engine at each step (default 120).
set -euo pipefail
cd "$(dirname "$0")/.."

timeout=${RFQ_DEMO_TIMEOUT:-120}
out=$(mktemp)
trap 'rm -f "$out"' EXIT

step() { printf '\n==> %s\n' "$*"; }

fail() {
  echo "FAIL: $*" >&2
  echo "--- engine log (last 30 lines) ---" >&2
  docker compose logs --no-log-prefix --tail 30 engine >&2 || true
  exit 1
}

engine_log() { docker compose logs --no-log-prefix engine 2>/dev/null; }

# "<seq> <hash>" from the engine's last log line `<what> seq=<n> hash=<hex>` (<what>: an ERE).
last_state() {
  engine_log | grep -E " ($1) seq=[0-9]+ hash=[0-9a-f]+" | tail -n 1 |
    sed -E 's/.* seq=([0-9]+) hash=([0-9a-f]+).*/\1 \2/' || true
}

# How many times the engine has started: it logs `replayed` once per start, after its replay.
starts() { engine_log | grep -cE ' replayed seq=[0-9]+ hash=' || true; }

engine_running() {
  local id
  id=$(docker compose ps -aq engine)
  [ -n "$id" ] && [ "$(docker inspect -f '{{.State.Running}}' "$id")" = true ]
}

# Waits until the engine has logged its <n>th `replayed` line; fails at once if it exits.
wait_started() {
  local n=$1 deadline=$((SECONDS + timeout))
  until [ "$(starts)" -ge "$n" ]; do
    engine_running || fail "the engine exited before it finished replaying its journal"
    [ "$SECONDS" -lt "$deadline" ] || fail "the engine did not finish starting within ${timeout}s"
    sleep 0.5
  done
}

# Waits until the engine's last logged state has not changed for 2 s, so no command or timer is in
# flight and the last `applied` line is the state its journal holds.
wait_idle() {
  local deadline=$((SECONDS + timeout)) prev="" cur stable=0
  while [ "$stable" -lt 4 ]; do
    sleep 0.5
    engine_running || fail "the engine exited"
    cur=$(last_state 'applied|replayed')
    if [ "$cur" = "$prev" ]; then stable=$((stable + 1)); else stable=0 prev=$cur; fi
    [ "$SECONDS" -lt "$deadline" ] || fail "the engine did not go idle within ${timeout}s"
  done
}

docker compose version >/dev/null 2>&1 || { echo "this script needs Docker with Compose v2" >&2; exit 1; }

step "Building the images (the first build compiles Aeron C and takes several minutes)"
docker compose --profile demo build

step "Starting a fresh stack: media driver + Archive, then the engine"
docker compose --profile demo down -v --remove-orphans
docker compose up -d aeron engine
wait_started 1
echo "engine started: seq+hash = $(last_state replayed)"

step "Running the scenarios over Aeron"
docker compose run --rm -T demo | tee "$out" || fail "the demo client failed"
summary=$(grep -E '^passed [0-9]+/[0-9]+$' "$out" | tail -n 1 || true)
passed=${summary#passed }
[ -n "$summary" ] && [ "${passed%/*}" -gt 0 ] && [ "${passed%/*}" = "${passed#*/}" ] &&
  ! grep -q '^FAIL ' "$out" || fail "not every scenario passed (${summary:-no summary})"

step "Crash: SIGKILL the engine once it is idle, then restart it"
wait_idle
started=$(starts)
docker compose kill -s SIGKILL engine
# Read after the kill: the dead process can log nothing more.
before=$(last_state 'applied|replayed')
[ -n "$before" ] || fail "the engine logged no state before the crash"
docker compose start engine
wait_started $((started + 1))
after=$(last_state replayed)

echo "before crash: seq+hash = $before"
echo "after replay: seq+hash = $after"
if [ "$before" = "$after" ]; then
  echo "RECOVERY OK"
else
  fail "RECOVERY MISMATCH: the replayed state differs from the state before the crash"
fi

step "Done. The stack is still running."
echo "Re-run the scenarios:  docker compose run --rm demo [scenario]"
echo "Stop everything:       docker compose --profile demo down -v"
