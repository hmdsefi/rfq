#!/usr/bin/env bash
# Builds dist/rfq-dashboard.html: the engine compiled to wasm, smoke-tested in Node, and inlined
# into one self-contained page. Demo aid; not part of the engine.
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
node web/driver.test.mjs
node web/scenarios.test.mjs "$WASM"
mkdir -p dist
# Each NAME=file.js fills the template's /*__NAME_JS__*/ marker (web/inline.mjs). The page template
# carries one marker per file listed here, no more and no fewer.
node web/inline.mjs "$WASM" web/index.html dist/rfq-dashboard.html \
  RUNTIME=web/runtime.js
