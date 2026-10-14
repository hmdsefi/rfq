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
const scripted = presets.filter((p) => p.kind !== "sandbox");
const sandboxes = presets.filter((p) => p.kind === "sandbox");
// The catalogue size is pinned: a dropped or renamed preset must not turn the gate green.
if (scripted.length !== 12 || sandboxes.length !== 1) {
  fail(`catalogue has ${scripted.length} presets and ${sandboxes.length} sandbox, expected 12 and 1`);
}
for (const p of scripted) {
  // A failed reset leaves the previous session loaded, so its old verdict must never count here.
  const loaded = call({ op: "reset", mode: p.name });
  if (loaded.error) {
    fail(`${p.name} did not load: ${loaded.error}`);
    continue;
  }
  const out = call({ op: "run_all" });
  const replay = call({ op: "replay_check" }).replay;
  if (out.preset?.name === p.name && out.preset?.verdict?.pass && replay?.matches && out.snapshot.invariants.ok) {
    console.log(`PASS ${p.name}`);
  } else {
    fail(`${p.name} (loaded: ${out.preset?.name}) ${JSON.stringify(out.error ?? out.preset?.verdict)}`);
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
