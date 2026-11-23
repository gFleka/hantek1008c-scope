#!/usr/bin/env bash
# Regression suite on the simulator (perf/TEST_PLAN.md): build + every test, PASS/FAIL summary, exit code.
#   scripts/regress.sh          build, cargo test --release (≈ 10 s of tests after the build)
#   scripts/regress.sh --long   + the long simulator runs (#[ignore = "long…"]) + `soak 5`
# Always runs with an isolated HOME / XDG_CONFIG_HOME: the owner's ~/.config and ~/Documents are never touched
# (checked at the end).
set -u
cd "$(dirname "$0")/.."
LONG=0
[ "${1:-}" = "--long" ] && LONG=1

REAL_HOME="$HOME"
export CARGO_HOME="${CARGO_HOME:-$REAL_HOME/.cargo}" RUSTUP_HOME="${RUSTUP_HOME:-$REAL_HOME/.rustup}"
export PATH="$CARGO_HOME/bin:$PATH"
REAL_CFG="${XDG_CONFIG_HOME:-$REAL_HOME/.config}/hantek1008c"
stamp() { find "$REAL_CFG" -maxdepth 1 -type f -printf '%p %T@\n' 2>/dev/null | sort; }
CFG_BEFORE=$(stamp)

TMP=$(mktemp -d /tmp/h8-regress.XXXXXX)
export HOME="$TMP/home" XDG_CONFIG_HOME="$TMP/config"
unset APPDATA USERPROFILE
mkdir -p "$HOME" "$XDG_CONFIG_HOME"

declare -a RESULTS
FAIL=0
step() { # name, command…
    local name=$1
    shift
    local t0=$SECONDS
    if "$@" >"$TMP/$name.log" 2>&1; then
        RESULTS+=("PASS  $name ($((SECONDS - t0)) s)")
    else
        RESULTS+=("FAIL  $name ($((SECONDS - t0)) s) — log: $TMP/$name.log")
        FAIL=1
        tail -n 40 "$TMP/$name.log"
    fi
}

step build cargo build --release
step tests cargo test --release
if [ "$LONG" = 1 ]; then
    step long-tests cargo test --release regress::sim::long_ -- --ignored
    step soak-5min ./target/release/hantek1008c soak 5 "$TMP/soak"
fi

CFG_AFTER=$(stamp)
if [ "$CFG_BEFORE" = "$CFG_AFTER" ]; then
    RESULTS+=("PASS  real config untouched ($REAL_CFG)")
else
    RESULTS+=("FAIL  real config changed: $REAL_CFG")
    FAIL=1
fi

echo
echo "── regression summary ──"
if [ -f "$TMP/tests.log" ]; then
    grep -E "^test result:" "$TMP/tests.log" | sed 's/^/      /'
    BUGS=$(grep -cE "ignored, BUG" "$TMP/tests.log")
    echo "      known bugs (ignored tests marked BUG): $BUGS"
    grep -E "ignored, BUG" "$TMP/tests.log" | sed 's/^test /        /; s/ \.\.\. ignored, / — /'
fi
printf '%s\n' "${RESULTS[@]}"
if [ "$FAIL" = 0 ]; then
    echo "PASS"
    rm -rf "$TMP"
else
    echo "FAIL (logs kept in $TMP)"
fi
exit $FAIL
