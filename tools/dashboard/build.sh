#!/usr/bin/env bash
# Builds dist/rfq-dashboard.html, the live page (DESIGN §11): one self-contained file that drives the
# real stack through rfq-gateway. The engine is still compiled to wasm here: the Node smoke test and the
# check that the JS scenario scripts match the Rust presets run it. The page itself carries no wasm.
# Demo aid; not part of the engine.
set -euo pipefail
cd "$(dirname "$0")"

# A failed build or smoke run must not leave an older page behind that looks fresh.
rm -f dist/rfq-dashboard.html

# No `grep -q`: it exits at the first match and can SIGPIPE rustup under pipefail.
if ! rustup target list --installed | grep -x wasm32-unknown-unknown >/dev/null; then
  rustup target add wasm32-unknown-unknown
fi
cargo build --release --target wasm32-unknown-unknown
WASM=target/wasm32-unknown-unknown/release/rfq_dashboard.wasm

node web/smoke.mjs "$WASM"
# The page's scripts and the inliner, tested without a network: the live driver itself is checked
# against the Docker stack by web/live-smoke.mjs.
node web/inline.test.mjs
node web/recorder.test.mjs
node web/model.test.mjs
node web/driver.test.mjs
node web/page.test.mjs
node web/scenarios.test.mjs "$WASM"
# The runs a reload brings back from the history, against the driver's own record of them (wasm engine).
node web/history.test.mjs "$WASM"
mkdir -p dist
# Each NAME=file.js fills the template's /*__NAME_JS__*/ marker (web/inline.mjs). The page template
# carries one marker per file listed here, no more and no fewer; `-` means no wasm in the page.
node web/inline.mjs - web/index.html dist/rfq-dashboard.html \
  RECORDER=web/recorder.js GATEWAY=web/gateway.js SCENARIOS=web/scenarios.js MODEL=web/model.js
