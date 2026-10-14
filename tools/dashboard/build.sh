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
mkdir -p dist
node web/inline.mjs "$WASM" web/index.html web/runtime.js dist/rfq-dashboard.html
