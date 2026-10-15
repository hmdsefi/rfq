// Checks the JS scripts against their Rust source, with no network. The Rust presets (the testkit's
// six scenarios and the six attacks, `src/presets.rs`) run in the dashboard's wasm engine and leave a
// trace: every command with its actor, nonce, action and time. Each JS script, resolved step by step
// the way the driver does, must produce exactly those commands, and its own events must pass its
// `expect` list.
//
// Usage: node web/scenarios.test.mjs [path/to/rfq_dashboard.wasm]
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const { createEngine } = require("./runtime.js");
const R = require("./recorder.js");
const S = require("./scenarios.js");

const wasmPath = process.argv[2] ?? fileURLToPath(new URL("../target/wasm32-unknown-unknown/release/rfq_dashboard.wasm", import.meta.url));
const { call } = await createEngine(readFileSync(wasmPath));

/** Every preset starts its clock here, so its contract id is 1000 (`presets::T0`). */
const T0 = 1_000;
let failed = 0;
function test(name, fn) {
  try {
    fn();
    console.log(`ok   ${name}`);
  } catch (e) {
    failed++;
    console.log(`FAIL ${name}: ${e.message}`);
  }
}

const { presets } = call({ op: "presets" });
const rust = presets.filter((p) => p.kind !== "sandbox");

test("the same 12 scenarios, in the same order, with the same kind, summary and expect list", () => {
  assert.deepEqual(S.names(), rust.map((p) => p.name));
  for (const p of rust) {
    const js = S.byName(p.name);
    assert.equal(js.kind, p.kind, `${p.name}: kind`);
    assert.equal(js.summary, p.summary, `${p.name}: summary`);
    assert.deepEqual(js.expect, p.expect, `${p.name}: expect`);
    assert.equal(js.steps.length, p.steps, `${p.name}: step count`);
  }
});

for (const p of rust) {
  const js = S.byName(p.name);
  const loaded = call({ op: "reset", mode: p.name });
  assert.ok(!loaded.error, loaded.error);
  const done = call({ op: "run_all" });
  assert.equal(done.preset.verdict.pass, true, `${p.name}: the Rust preset itself does not pass`);
  // Entry 0 is the genesis; then one entry per step, in order.
  const trace = done.trace.slice(1);

  test(`${p.name}: every step is the Rust step (time, actor, nonce, action)`, () => {
    assert.equal(trace.length, js.steps.length);
    const ctx = new S.Ctx(T0);
    // What the gateway does for an ordinary command: the actor's last consumed nonce + 1.
    const consumed = new Map();
    const lastOrdinary = new Map();
    const unconsumed = (events) => events.some((e) => ["Duplicate", "NonceGap"].includes(R.eventBody(e).reason));
    js.steps.forEach((step, i) => {
      const entry = trace[i];
      const where = `step ${i} (${step.label ?? "wait"} at +${step.at})`;
      assert.equal(entry.time, T0 + step.at, `${where}: time`);
      if (step.wait) {
        assert.equal(entry.kind, "tick", `${where}: a wait is a Tick`);
        assert.equal(entry.command, "Tick", `${where}: a wait is a Tick`);
      } else {
        const user = entry.command.User;
        assert.ok(user, `${where}: a command step is a user command`);
        const cmd = S.resolveStep(step, ctx, (actor) => lastOrdinary.get(actor));
        ctx.recordLabel(step.label, entry.seq);
        const nonce = cmd.nonce ?? (consumed.get(cmd.actor) ?? 0) + 1;
        assert.equal(cmd.actor, user.actor, `${where}: actor`);
        assert.equal(nonce, user.nonce, `${where}: nonce`);
        assert.deepEqual(cmd.action, user.action, `${where}: action`);
        if (cmd.nonce === undefined) lastOrdinary.set(cmd.actor, nonce);
        const rejections = entry.events.filter((e) => R.eventName(e) === "CommandRejected");
        if (!unconsumed(rejections)) consumed.set(cmd.actor, nonce);
      }
      // The driver notes the presented picks from every batch, the step's or a timer's.
      ctx.observe(entry.events);
    });
  });

  test(`${p.name}: the run's events (scope) pass the expect list, and nothing is unclassified`, () => {
    const scope = new R.Scope(T0);
    // The Rust preset runs on the well-known account ids; the driver's scope maps them, so own them here.
    for (const a of [4, 10, 20, 21, 30]) scope.accounts.add(a);
    const own = [];
    for (const entry of trace) {
      const user = entry.command.User;
      if (user) scope.sent(user.actor, user.nonce);
      const batch = { seq: entry.seq, origin: user ? { actor: user.actor, nonce: user.nonce } : null };
      for (const e of entry.events) if (scope.owns(batch, e)) own.push(e);
    }
    assert.equal(scope.unclassified.size, 0, [...scope.unclassified].join(", "));
    assert.deepEqual(R.containsInOrder(own, js.expect), { ok: true, error: null });
    // The expect list is not trivially met: a rejection pinned to another reason is not.
    assert.equal(R.containsInOrder(own, [...js.expect, "CommandRejected(Overflow)"]).ok, false);
  });
}

test("a scenario's accounts, once mapped, are the run's own (only OPERATOR and ARBITER are shared)", () => {
  for (const js of S.scenarios) {
    const scope = new R.Scope(7_000);
    const ctx = new S.Ctx(7_000, (a) => scope.account(a));
    const seen = new Set();
    js.steps.forEach((step, i) => {
      if (step.wait) return;
      ctx.recordLabel(step.label, 100 + i);
      const cmd = S.resolveStep(step, ctx, () => 1);
      const body = Object.values(cmd.action)[0];
      for (const a of [cmd.actor, body.to, body.oracle].filter((x) => x !== undefined)) seen.add(a);
    });
    for (const a of seen) assert.ok(a < 4 || (a >= 700_000 && a < 700_100), `${js.name}: account ${a} is neither a genesis role nor the run's`);
    assert.ok(!seen.has(10) && !seen.has(20) && !seen.has(30), `${js.name}: a logical account leaked into a command`);
  }
});

test("remap maps the Deposit target and the oracle, and refuses an action it does not know", () => {
  const f = (a) => a + 1_000;
  assert.deepEqual(S.remap({ Deposit: { to: 10, amount: 5 } }, f), { Deposit: { to: 1_010, amount: 5 } });
  const listed = S.remap({ ListContract: { id: 9, description: "d", oracle: 4, trading_close: 1, resolution_deadline: 2, dispute_window: 3 } }, f);
  assert.deepEqual(listed, { ListContract: { id: 9, description: "d", oracle: 1_004, trading_close: 1, resolution_deadline: 2, dispute_window: 3 } });
  const cancel = { CancelRequest: { request: 9 } };
  assert.deepEqual(S.remap(cancel, f), cancel);
  assert.throws(() => S.remap({ Mint: { amount: 1 } }, f), /does not know the action Mint/);
});

test("ctx: contract, at, labels, picks", () => {
  const ctx = new S.Ctx(5_000);
  assert.equal(ctx.contract(), 5_000);
  assert.equal(ctx.at(1_500), 6_500);
  assert.throws(() => ctx.request("req"), /label req has no seq yet/);
  ctx.recordLabel("req", 42);
  assert.equal(ctx.request("req"), 42);
  assert.equal(ctx.quote("req"), 42);
  assert.deepEqual(ctx.picks(42), []);
  const picks = [{ leg: 0, quote: 44, price: 6_000 }];
  ctx.observe([{ audience: { Account: 10 }, event: { RequestPresented: { request: 42, picks, accept_by: 1 } } }]);
  assert.deepEqual(ctx.picks(42), picks);
});

test("resolveStep: ordinary steps leave the nonce to the gateway; reuse and skip count from the last ordinary one", () => {
  const ctx = new S.Ctx(1_000);
  const step = (nonce) => ({ at: 0, label: "x", actor: 20, action: () => ({ Withdraw: { amount: 0 } }), nonce });
  assert.equal("nonce" in S.resolveStep(step(undefined), ctx, () => 5), false);
  assert.equal(S.resolveStep(step("reuse"), ctx, () => 5).nonce, 5);
  assert.equal(S.resolveStep(step("skip"), ctx, () => 5).nonce, 7);
  assert.throws(() => S.resolveStep(step("reuse"), ctx, () => undefined), /needs an earlier command/);
  assert.throws(() => S.resolveStep(step("later"), ctx, () => 5), /unknown nonce rule/);
});

console.log(failed ? `${failed} FAILED` : "all scenario script checks passed");
process.exit(failed ? 1 : 0);
