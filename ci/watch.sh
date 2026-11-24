#!/usr/bin/env bash
# The watch: this Plugin against what Yoke last published. The newest programs are installed from the module
# proxy, the Plugin is built and taken into try/ by them as anyone's would be, run under a real Core, and asked
# its status. A runner may have no UPower, so there the question is failed as unavailable; on a host
# with one, it is answered. Either is the Plugin admitted, running and answering under the release.
# Usage: watch.sh [results directory]
set -uo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
results="${1:-$root/.results}"
mkdir -p "$results" && results="$(cd "$results" && pwd)" # absolute: the script moves into try/
date -u +%Y-%m-%dT%H:%M:%SZ > "$results/started"
CASE=yoke-plugin-power:the-watch.01

finish() { # <verdict line>...
  printf '%s\n' "$@" | tee "$results/checks.txt"
  date -u +%Y-%m-%dT%H:%M:%SZ > "$results/finished"
  ! grep -qv '^pass' "$results/checks.txt"
  exit $?
}

# Short, since the longest socket path of the instance is computed under it and bounded.
work="$(mktemp -d /tmp/w.XXX)"
trap 'kill "${running:-0}" 2>/dev/null; rm -rf "$work"' EXIT
mkdir -p "$work/bin" "$work/r" "$work/state" "$work/config/yoke"
export GOBIN="$work/bin" PATH="$work/bin:$PATH" XDG_RUNTIME_DIR="$work/r" XDG_STATE_HOME="$work/state" XDG_CONFIG_HOME="$work/config"
printf 'install_root: %s\n' "$work/apps" > "$work/config/yoke/manager.conf"

release="$(go list -m -f '{{.Version}}' github.com/yoke-project/yoke@latest)" || finish "FAIL  $CASE — the newest release of yoke cannot be read"
echo "watch: yoke $release"
echo "$release" > "$results/release"
for program in yoke yoke-compose yoke-core yoke-guardian yoke-auth; do
  go install "github.com/yoke-project/yoke/cmd/$program@$release" || finish "FAIL  $CASE — $program $release does not install"
done

cd "$root/try" || finish "FAIL  $CASE — no try/"
yoke-compose remove units.power > /dev/null 2>&1
rm -f components/plugins/power manifests/com.yoke.power.yaml
said="$(yoke-compose add plugin .. --yes 2>&1)" || finish "FAIL  $CASE — the Plugin is not taken under $release: $(tail -n 2 <<<"$said" | tr '\n' ' ')"

cd "$root" || exit 1
yoke app run --from try > "$work/run.log" 2>&1 &
running=$!
for _ in $(seq 1 30); do
  grep -q 'is observable' "$work/run.log" && break
  sleep 1
done
grep -q 'is observable' "$work/run.log" || finish "FAIL  $CASE — the instance did not come up under $release: $(tail -n 3 "$work/run.log" | tr '\n' ' ')"
instance="$(sed -n '1s/ runs .*//p' "$work/run.log" | head -n 1)"

# std: yoke-plugin-power:the-watch.01
probe() { # <instance>; says why on a failure
  yoke plugin grant com.yoke.power query.power.status.answer --instance "$1" > /dev/null || { echo "the grant was refused"; return 1; }
  yoke unit restart power --instance "$1" > /dev/null || { echo "the unit did not restart"; return 1; }
  sleep 3
  local said
  if said="$(yoke unit ask power power.status --instance "$1" 2>&1)"; then
    grep -q '"sequence"' <<<"$said" || { echo "the answer holds no sequence: $said"; return 1; }
  else
    grep -q 'unavailable' <<<"$said" || { echo "the question was refused, and not for want of a manager: $said"; return 1; }
  fi
}

if probe "$instance" > "$work/why"; then verdict="pass  $CASE"; else verdict="FAIL  $CASE — under $release: $(cat "$work/why")"; fi

# The panel in try/ui, built against the published library and run with no screen — X under Xvfb, drawn by
# Slint's software renderer — on the application's channel `panel` of the same instance. It attaches when it
# says so; it says so on stderr as in its window.
PANEL=yoke-plugin-power:the-watch.02
# std: yoke-plugin-power:the-watch.02
panel() { # <instance>; says why on a failure
  command -v xvfb-run > /dev/null || { echo "xvfb-run is not on this host"; return 1; }
  local built
  built="$(cd "$root/try/ui" && CARGO_TARGET_DIR="$work/ui" cargo build --release --locked 2>&1)" || { echo "the panel does not build: $(tail -n 2 <<<"$built" | tr '\n' ' ')"; return 1; }
  SLINT_BACKEND=winit-software timeout 80 xvfb-run -a "$work/ui/release/power-ui" --instance "$1" --channel panel 2> "$work/panel.log" &
  local shown=$!
  for _ in $(seq 1 60); do
    grep -q 'power-ui: Attached' "$work/panel.log" && break
    sleep 1
  done
  kill "$shown" 2>/dev/null
  wait "$shown" 2>/dev/null
  grep -q 'power-ui: Attached' "$work/panel.log" || { echo "the panel did not attach: $(tail -n 3 "$work/panel.log" | tr '\n' ' ')"; return 1; }
}
if panel "$instance" > "$work/why"; then verdict2="pass  $PANEL"; else verdict2="FAIL  $PANEL — under $release: $(cat "$work/why")"; fi

kill -INT "$running" 2>/dev/null
for _ in $(seq 1 30); do kill -0 "$running" 2>/dev/null || break; sleep 1; done
finish "$verdict" "$verdict2"
