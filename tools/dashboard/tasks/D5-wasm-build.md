# Task D5: WASM build, Node smoke test, inliner

**Goal:** `./build.sh` compiles the crate to `wasm32-unknown-unknown` and proves the binary works headless in Node: every preset reaches PASS, replays match, and bad input returns an error. It then inlines the WASM into `dist/rfq-dashboard.html`. The page template is a placeholder in this task; D6 replaces it with the real page.

**Files:**
- Create:
  - `tools/dashboard/build.sh` (executable);
  - `tools/dashboard/web/runtime.js`: the loader and RPC, shared by the page and the smoke test;
  - `tools/dashboard/web/smoke.mjs`;
  - `tools/dashboard/web/inline.mjs`;
  - `tools/dashboard/web/index.html`: a temporary minimal template; D6 replaces it.
- Modify: none.

**Interfaces:**
- Consumes: the D4 ABI (`in_buf`, `handle`, `out_ptr`, `memory`) and the JSON protocol.
- Produces, used by D6 and D7:
  - `web/runtime.js` defines `createEngine(wasmBytes) -> Promise<{ call(request) -> response }>`. `call` takes a plain JS object and returns the parsed JSON response.
  - `build.sh` replaces two markers in `web/index.html`:
    - `/*__RUNTIME_JS__*/` with the content of `web/runtime.js`;
    - `__WASM_BASE64__` with the base64 WASM.

## Steps

- [ ] **Step 1: Write `web/runtime.js`.** It's plain script, not a module, so it can be inlined into a `<script>` tag and also loaded by Node.

```js
// The page's only bridge to the engine: JSON in, JSON out over the 3-function WASM ABI
// (in_buf, handle, out_ptr). Shared by the page and by the Node smoke test.
async function createEngine(wasmBytes) {
  const { instance } = await WebAssembly.instantiate(wasmBytes, {});
  const ex = instance.exports;
  const encoder = new TextEncoder();
  const decoder = new TextDecoder();
  function call(request) {
    const bytes = encoder.encode(JSON.stringify(request));
    const at = ex.in_buf(bytes.length);
    // Views are made after each export call: the call may have grown (and moved) memory.
    new Uint8Array(ex.memory.buffer, at, bytes.length).set(bytes);
    const n = ex.handle();
    const out = new Uint8Array(ex.memory.buffer, ex.out_ptr(), n);
    return JSON.parse(decoder.decode(out));
  }
  return { call };
}
if (typeof module !== "undefined") module.exports = { createEngine };
```

- [ ] **Step 2: Write `web/smoke.mjs`.**

```js
// Headless check of the built wasm: every preset passes and replays to the same hash,
// bad input returns an error. Usage: node web/smoke.mjs <path/to/rfq_dashboard.wasm>
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const { createEngine } = require("./runtime.js");

const wasm = readFileSync(process.argv[2]);
const { call } = await createEngine(wasm);
let failed = 0;
const fail = (msg) => { console.log(`FAIL ${msg}`); failed++; };

const { presets } = call({ op: "presets" });
for (const p of presets.filter((p) => p.kind !== "sandbox")) {
  call({ op: "reset", mode: p.name });
  const out = call({ op: "run_all" });
  const replay = call({ op: "replay_check" }).replay;
  if (out.preset?.verdict?.pass && replay?.matches && out.snapshot.invariants.ok) {
    console.log(`PASS ${p.name}`);
  } else {
    fail(`${p.name} ${JSON.stringify(out.error ?? out.preset?.verdict)}`);
  }
}
if (!call({ op: "fly" }).error) fail("unknown op did not return an error");
if (!call({ op: "act", actor: 20, action: { SubmitQuote: { request: 1, leg: 0, price: 0, size: 1, expires_at: 1 } } }).error) {
  fail("price 0 did not return an error");
}
const sandbox = call({ op: "reset", mode: "sandbox" });
if (sandbox.snapshot.accounts.length !== 8) fail("sandbox does not show 8 accounts");
console.log(failed ? `${failed} FAILED` : "all smoke checks passed");
process.exit(failed ? 1 : 0);
```

- [ ] **Step 3: Write `web/inline.mjs`.**

```js
// Builds the single self-contained page: runtime.js and the base64 wasm inlined into the template.
// Usage: node web/inline.mjs <wasm> <template.html> <runtime.js> <out.html>
import { readFileSync, writeFileSync } from "node:fs";

const [wasmPath, templatePath, runtimePath, outPath] = process.argv.slice(2);
let html = readFileSync(templatePath, "utf8");
for (const marker of ["/*__RUNTIME_JS__*/", "__WASM_BASE64__"]) {
  if (!html.includes(marker)) throw new Error(`template is missing ${marker}`);
}
const b64 = readFileSync(wasmPath).toString("base64");
// Function replacers: the inlined text is used verbatim (no `$&` patterns).
html = html.replace("/*__RUNTIME_JS__*/", () => readFileSync(runtimePath, "utf8"));
html = html.replace("__WASM_BASE64__", () => b64);
writeFileSync(outPath, html);
console.log(`wrote ${outPath}: ${Math.round(html.length / 1024)} KiB (wasm ${Math.round(b64.length / 1024)} KiB as base64)`);
```

- [ ] **Step 4: Write the temporary `web/index.html`.** D6 replaces it, but it must work now so the build is end-to-end.

```html
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>RFQ Engine Dashboard</title>
</head>
<body>
<p>Demo aid — not part of the engine.</p>
<pre id="out">loading…</pre>
<script>/*__RUNTIME_JS__*/</script>
<script>
const WASM_BASE64 = "__WASM_BASE64__";
(async () => {
  const bytes = Uint8Array.from(atob(WASM_BASE64), (c) => c.charCodeAt(0));
  const engine = await createEngine(bytes);
  const out = engine.call({ op: "state" });
  document.getElementById("out").textContent =
    `seq ${out.snapshot.seq} · hash ${out.snapshot.state_hash.slice(0, 12)} · invariants ${out.snapshot.invariants.ok ? "ok" : "BROKEN"}`;
})().catch((e) => { document.getElementById("out").textContent = `failed to start: ${e}`; });
</script>
</body>
</html>
```

- [ ] **Step 5: Write `build.sh`.**

```bash
#!/usr/bin/env bash
# Builds dist/rfq-dashboard.html: the engine compiled to wasm, smoke-tested in Node, and inlined
# into one self-contained page. Demo aid; not part of the engine.
set -euo pipefail
cd "$(dirname "$0")"

if ! rustup target list --installed | grep -qx wasm32-unknown-unknown; then
  rustup target add wasm32-unknown-unknown
fi
cargo build --release --target wasm32-unknown-unknown
WASM=target/wasm32-unknown-unknown/release/rfq_dashboard.wasm

node web/smoke.mjs "$WASM"
mkdir -p dist
node web/inline.mjs "$WASM" web/index.html web/runtime.js dist/rfq-dashboard.html
```

Run: `chmod +x tools/dashboard/build.sh`

- [ ] **Step 6: Build and run it.**

Run: `tools/dashboard/build.sh`
Expected:
- 12 `PASS <name>` lines, then `all smoke checks passed`;
- `wrote dist/rfq-dashboard.html: … KiB`;
- exit status 0.

If the WASM imports anything (Node throws `LinkError` or `TypeError: import object field …`), run `wasm-objdump -x` if it's installed, or look in `node -e` at `WebAssembly.Module.imports(new WebAssembly.Module(fs.readFileSync(path)))`. Remove the dependency that pulls the import in. `serde_json`, `bincode` and `sha2` on wasm32 normally import nothing.

- [ ] **Step 7: Check the WASM has no imports and the page opens.**

Run: `node -e 'const fs=require("fs");const m=new WebAssembly.Module(fs.readFileSync(process.argv[1]));console.log("imports:",WebAssembly.Module.imports(m).length,"exports:",WebAssembly.Module.exports(m).map(e=>e.name).join(","))' tools/dashboard/target/wasm32-unknown-unknown/release/rfq_dashboard.wasm`
Expected: `imports: 0 exports: memory,in_buf,handle,out_ptr` (the order may differ).

Then open `tools/dashboard/dist/rfq-dashboard.html` in the built-in browser (`file://` URL). Expected: a line like `seq 6 · hash … · invariants ok` and no console errors.

- [ ] **Step 8: Commit** (never `dist/` or `target/`).

```bash
git add tools/dashboard/build.sh tools/dashboard/web/runtime.js tools/dashboard/web/smoke.mjs tools/dashboard/web/inline.mjs tools/dashboard/web/index.html
git commit -m "build(dashboard): wasm build, Node smoke test of every preset, single-file inliner"
```
