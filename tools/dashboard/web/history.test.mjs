// The runs a reloaded page finds in the read API's trace (`RfqScenarios.runsFromHistory`), checked against
// the driver's own record of the same runs. The real driver plays them, through the real gateway client, to
// a fake gateway whose engine is the dashboard's wasm build of the real one, in real time: each command is
// stamped with the wall clock, the timers fire when due, and every batch is kept as the admin trace would
// hold it. Usage: node web/history.test.mjs [path/to/rfq_dashboard.wasm]
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const { createEngine } = require("./runtime.js");
const G = require("./gateway.js");
const R = require("./recorder.js");
const S = require("./scenarios.js");

const wasmPath = process.argv[2] ?? fileURLToPath(new URL("../target/wasm32-unknown-unknown/release/rfq_dashboard.wasm", import.meta.url));
const { call } = await createEngine(readFileSync(wasmPath));

let failed = 0;
async function test(name, fn) {
  try {
    await fn();
    console.log(`ok   ${name}`);
  } catch (e) {
    failed++;
    console.log(`FAIL ${name}: ${e.stack?.split("\n").slice(0, 3).join(" | ") ?? e}`);
  }
}

// ---- a gateway over the wasm engine --------------------------------------------------------------------

const UNKNOWN = (nonce) =>
  `outcome unknown: the engine stopped answering after this command was sent; resend it with nonce ${nonce} to retry safely (a repeat is rejected as Duplicate)`;
const KINDS = { genesis: "Genesis", tick: "Tick", resume: "Resume" };
const unconsumed = (events) => events.some((e) => R.eventName(e) === "CommandRejected" && ["Duplicate", "NonceGap"].includes(R.eventBody(e).reason));

/** A wasm trace entry as the read API's admin trace holds it: the batch, and the command that made it. */
function entryOf(t) {
  const user = t.command?.User;
  return {
    seq: t.seq,
    time: t.time,
    kind: KINDS[t.kind] ?? "User",
    origin: user ? { actor: user.actor, nonce: user.nonce } : null,
    events: t.events,
    balances: [],
    command: user ? { actor: user.actor, nonce: user.nonce, action: user.action } : null,
  };
}

/**
 * A gateway whose engine is the wasm one. A command is applied at the wall clock's time, after the timers due
 * by then; the timers also fire by themselves when due, as the node's do. The gateway assigns the nonces
 * (the actor's last consumed one + 1). Every batch goes to the socket and into `entries`. `fault(msg)` changes
 * how one command is answered: `"late"` (it reaches the engine 2 s late), `"unavailable"` (refused, never
 * applied) or `"unknown"` (applied, then its answer is lost: `outcome unknown`, the engine down and up again).
 */
function wasmGateway(fault = () => null) {
  const loaded = call({ op: "reset", mode: "happy_path" }); // a fresh engine: only the genesis (the preset never runs)
  assert.ok(!loaded.error, loaded.error);
  const server = { sockets: [], entries: loaded.trace.map(entryOf) };
  const consumed = new Map();
  let clock = loaded.snapshot.now;
  let due = loaded.snapshot.next_timer;
  let timer = null;
  const deliver = (frame) => server.sockets.at(-1)?.onmessage?.({ data: JSON.stringify(frame) });

  /** One engine op; the batches it made go out (a tick that fired nothing is not one). */
  function apply(req) {
    const out = call(req);
    if (out.error) throw new Error(out.error);
    clock = out.snapshot.now;
    due = out.snapshot.next_timer;
    const made = out.trace.filter((t) => out.new_seqs.includes(t.seq)).map(entryOf);
    for (const e of made) {
      if (e.command && !unconsumed(e.events)) consumed.set(e.command.actor, e.command.nonce);
      if (!e.command && !e.events.length) continue;
      server.entries.push(e);
      const { command, ...batch } = e;
      deliver({ type: "batch", ...batch });
    }
    arm();
    return made;
  }
  /** The engine's clock to now: the timers due by then fire first, each at its own time. */
  function catchUp() {
    const now = Date.now();
    while (due != null && due <= now) apply({ op: "next_timer" });
    if (now > clock) apply({ op: "advance", ms: now - clock });
  }
  function arm() {
    clearTimeout(timer);
    timer = due == null ? null : setTimeout(catchUp, Math.max(0, due - Date.now()));
  }
  function command(msg) {
    const how = fault(msg);
    if (how === "unavailable") return deliver({ type: "error", id: msg.id, error: "engine unavailable" });
    catchUp();
    const nonce = msg.nonce ?? (consumed.get(msg.actor) ?? 0) + 1;
    deliver({ type: "sent", id: msg.id, actor: msg.actor, nonce });
    const [entry] = apply({ op: "raw", actor: msg.actor, nonce, action: msg.action });
    if (how === "unknown") {
      deliver({ type: "error", id: msg.id, error: UNKNOWN(nonce) });
      deliver({ type: "status", engine: "down", last_seq: entry.seq });
      setTimeout(() => deliver({ type: "status", engine: "up", last_seq: entry.seq }), 50);
      return;
    }
    deliver({ type: "result", id: msg.id, seq: entry.seq, latency_us: 100 });
  }
  server.Socket = class {
    constructor(url) {
      this.url = url;
      server.sockets.push(this);
      setTimeout(() => this.onopen?.({}), 0);
    }
    send(text) {
      const msg = JSON.parse(text);
      setTimeout(() => {
        if (msg.type === "hello") {
          deliver({ type: "welcome", view: msg.view, engine: "up", now: Date.now() });
          deliver({ type: "status", engine: "up", last_seq: server.entries.at(-1).seq });
        } else if (msg.type === "command") {
          setTimeout(() => command(msg), fault(msg) === "late" ? 2_000 : 0);
        }
      }, 0);
    }
    close() {
      this.onclose?.({});
    }
  };
  /** Fires every timer still pending, at once: the history then holds what the engine did after the runs. */
  server.drain = () => {
    for (let i = 0; due != null && i < 1_000; i++) apply({ op: "next_timer" });
    clearTimeout(timer);
  };
  server.stop = () => clearTimeout(timer);
  return server;
}

/** The label of the `n`th command step (from 1) of scenario `name`: the driver's command ids are `<run>.<n>`. */
const labelOf = (name, n) => S.byName(name).steps.filter((s) => !s.wait)[n - 1]?.label;

// ---- the runs, played live ------------------------------------------------------------------------------

/** The scenarios played at once, each a run with its own t0; three of them meet a fault. */
const PLAYED = ["happy_path", "multi_leg_happy", "attack_double_accept", "attack_accept_wrong_price", "attack_replayed_nonce", "requester_lapses"];
const FAULTS = {
  // MM2's quote is applied and its answer lost: the driver resends it with the same nonce, a Duplicate.
  multi_leg_happy: { q2: "unknown" },
  // Alice's accept reaches the engine after accept_by: rejected, and the run FAILs.
  attack_double_accept: { accept: "late" },
  // MM2's quote is refused: the run FAILs with its setup, its request and one quote sent.
  requester_lapses: { q2: "unavailable" },
};

const names = new Map(); // run id -> scenario name
const server = wasmGateway((msg) => {
  const [, run, n] = /^(r\d+)\.(\d+)$/.exec(msg.id ?? "") ?? [];
  const name = names.get(run);
  return name ? (FAULTS[name]?.[labelOf(name, Number(n))] ?? null) : null;
});
const gw = G.connect("ws://wasm", { WebSocket: server.Socket, connectTimeoutMs: 2_000 });
await gw.ready;
const driver = S.createDriver(gw, { onRunStart: (run) => names.set(run.id, run.name) });

/** The README's curl walk-through, by hand, in the middle of the runs: accounts 10 and 21, contract 7, all below a run's ids. */
async function walkThrough() {
  const now = Math.round(gw.engineNow());
  const seqs = [];
  const send = async (actor, action) => seqs.push((await gw.command({ actor, action })).result.seq);
  await send(1, { Deposit: { to: 10, amount: 1_000_000_000 } });
  await send(1, { Deposit: { to: 21, amount: 1_000_000_000 } });
  await send(1, { ListContract: { id: 7, description: "Will it rain in Paris on 2026-11-01?", oracle: 4, trading_close: now + 60_000, resolution_deadline: now + 120_000, dispute_window: 2_000 } });
  await send(10, { SubmitRequest: { legs: [{ contract: 7, side: "BuyYes", qty: 100, limit_price: 7_000 }], response_deadline: now + 1_000 } });
  await send(21, { SubmitQuote: { request: seqs.at(-1), leg: 0, price: 6_000, size: 100, expires_at: now + 10_000 } });
  return seqs;
}

const played = driver.runMany(PLAYED, 1, PLAYED.length);
await new Promise((resolve) => setTimeout(resolve, 700));
const curl = await walkThrough();
const live = (await played).runs;
server.drain();
driver.dispose();
gw.close();
const history = server.entries;
let rebuilt = [];
await test("the history rebuilds", () => {
  rebuilt = S.runsFromHistory(history);
});
const liveOf = (name) => live.find((r) => r.name === name);

await test("the runs played as the faults say: four PASS, the late accept and the refused quote FAIL, and MM2's lost answer was retried", () => {
  assert.deepEqual(live.map((r) => [r.name, r.pass]), PLAYED.map((n) => [n, !["attack_double_accept", "requester_lapses"].includes(n)]), live.map((r) => `${r.name}: ${r.detail}`).join("\n"));
  assert.match(liveOf("requester_lapses").detail, /q2 \(mm2 SubmitQuote\): engine unavailable/);
  assert.match(liveOf("attack_double_accept").detail, /expected event RequestFilled/);
  const [retry] = liveOf("multi_leg_happy").recoveries;
  assert.match(retry.outcome, /Duplicate on retry/);
  assert.ok(liveOf("multi_leg_happy").batches.includes(retry.retry_seq), "the retry's batch is the run's");
});

await test("one run per scenario run, by t0, each marked as history with an id no driver run has", () => {
  assert.deepEqual(rebuilt.map((r) => r.t0), live.map((r) => r.t0).sort((a, b) => a - b));
  for (const r of rebuilt) {
    assert.equal(r.history, true);
    assert.equal(r.id, `h${r.t0}`);
    assert.equal(r.done, true, "never running");
    assert.equal(r.stopped, false);
  }
});

await test("each run has its scenario's name, and the one cut short has the neutral label and no verdict", () => {
  const byT0 = new Map(rebuilt.map((r) => [r.t0, r]));
  for (const run of live.filter((r) => r.name !== "requester_lapses")) {
    assert.equal(byT0.get(run.t0).name, run.name, `t0 ${run.t0}`);
    assert.equal(byT0.get(run.t0).kind, run.kind);
  }
  // Its setup, its request and MM1's quote: the start of five scripts, the whole of none.
  const partial = byT0.get(liveOf("requester_lapses").t0);
  assert.match(partial.name, /^run \d\d:\d\d:\d\d$/);
  assert.equal(partial.pass, null);
  assert.equal(partial.detail, null);
  assert.deepEqual(partial.commands.map((c) => c.action), ["Deposit", "Deposit", "Deposit", "Deposit", "ListContract", "SubmitRequest", "SubmitQuote"]);
});

await test("the two scripts that send the same kinds in the same order are told apart by their actions: the wrong price and the double accept", () => {
  const byT0 = new Map(rebuilt.map((r) => [r.t0, r]));
  for (const name of ["attack_accept_wrong_price", "attack_double_accept"]) assert.equal(byT0.get(liveOf(name).t0).name, name);
});

await test("the verdicts are the live ones: PASS for the passing runs, FAIL with the same detail for the late accept", () => {
  const byT0 = new Map(rebuilt.map((r) => [r.t0, r]));
  for (const run of live.filter((r) => r.name !== "requester_lapses")) {
    const r = byT0.get(run.t0);
    assert.equal(r.pass, run.pass, `${run.name}: ${r.detail}`);
    assert.equal(r.detail, run.detail, run.name);
    assert.deepEqual([r.incomplete, r.open], [false, false], `${run.name}: the whole history holds it`);
  }
});

await test("own events, batches and labels are exactly what the driver recorded live for the same run", () => {
  const byT0 = new Map(rebuilt.map((r) => [r.t0, r]));
  for (const run of live.filter((r) => r.name !== "requester_lapses")) {
    const r = byT0.get(run.t0);
    assert.deepEqual(r.own, run.own, `${run.name}: own`);
    assert.deepEqual(r.batches, run.batches, `${run.name}: batches`);
    assert.deepEqual(r.labels, run.labels, `${run.name}: labels`);
    assert.deepEqual(r.commands.map((c) => [c.step, c.actor, c.action, c.nonce, c.seq, c.rejected, c.recovered]),
      run.commands.map((c) => [c.step, c.actor, c.action, c.nonce, c.seq, c.rejected, c.recovered]), `${run.name}: commands`);
  }
});

await test("not vacuous: the history goes on after the runs (the markets close, the contracts finalize), and none of it is the runs'", () => {
  const multi = liveOf("multi_leg_happy");
  const market = multi.t0 * 10 + 1;
  const after = history.filter((e) => e.events.some((x) => ["ContractClosed", "ContractFinalized"].includes(R.eventName(x)) && R.eventBody(x).contract === market));
  assert.equal(after.length, 2, "the market closed and finalized in the history");
  const r = rebuilt.find((x) => x.t0 === multi.t0);
  for (const e of after) assert.ok(!r.batches.includes(e.seq), `seq ${e.seq} is not the run's`);
});

await test("a command that belongs to no run is in no run: the curl walk-through's accounts and contract are below a run's ids", () => {
  assert.equal(curl.length, 5);
  for (const r of rebuilt) {
    for (const seq of curl) assert.ok(!r.batches.includes(seq), `seq ${seq} in ${r.name}`);
    assert.ok(r.commands.every((c) => c.actor < 4 || c.actor >= r.t0 * 100), `${r.name}: a command of an account below a run's`);
  }
  assert.ok(![...S.runT0s(history)].some((t0) => t0 < 1_000), "no run of the walk-through's accounts");
});

await test("a run that stopped where another script's commands end is checked against that script's actions, not only its shape", () => {
  // multi_leg_happy stopped after MM2's quote (on leg 1): the shape of multi_leg_leg2_fails, whose MM2 quotes leg 2.
  // attack_accept_wrong_price stopped after its bad accept: the shape of oracle_silent_hard_deadline, whose accept takes the presented price.
  const multi = liveOf("multi_leg_happy");
  const wrong = liveOf("attack_accept_wrong_price");
  const dropped = new Set([multi.labels.q3, multi.labels.accept, wrong.labels.accept]);
  const stopped = S.runsFromHistory(history.filter((e) => !dropped.has(e.seq)));
  for (const run of [multi, wrong]) {
    const r = stopped.find((x) => x.t0 === run.t0);
    assert.match(r.name, /^run \d\d:\d\d:\d\d$/, `${run.name} stopped early is not ${r.name}`);
    assert.equal(r.pass, null);
  }
});

// ---- a history cut by a reload in the middle of a run ---------------------------------------------------

/**
 * The history as a reload `ms` after `run`'s t0 fetched it, whole up to then (`end`), and the batches recorded
 * after it, as the live stream carries them: without their command.
 */
function cutAt(run, ms) {
  const end = run.t0 + ms;
  const cut = history.filter((e) => e.time <= end);
  const after = history.filter((e) => e.time > end).map(({ command, ...batch }) => batch);
  return { cut, after, end };
}
const rebuiltIn = ({ cut, end }, run) => S.runsFromHistory(cut, { end }).find((r) => r.t0 === run.t0);

await test("a history cut while a run was still sending its commands never judges it: before the quotes, after them, after the accept", () => {
  const happy = liveOf("happy_path");
  for (const ms of [200, 1_000, 3_000]) {
    const fetched = cutAt(happy, ms);
    const { cut, after } = fetched;
    const r = rebuiltIn(fetched, happy);
    assert.equal(r.incomplete, true, `+${ms}`);
    assert.equal(r.open, false, `+${ms}: nothing will finish it`);
    assert.equal(r.pass, null, `+${ms}: ${r.detail}`);
    // Its commands so far start several scripts (and are the whole of one, at +1 s and +3 s): no name.
    assert.match(r.name, /^run \d\d:\d\d:\d\d$/, `+${ms}: ${r.name}`);
    assert.match(r.log.at(-1), /^incomplete: /);
    // The batches recorded after the reload do not judge it either.
    const later = S.settleRun(r, [...cut, ...after], happy.t0 + 60_000);
    assert.deepEqual([later.incomplete, later.pass], [true, null], `+${ms}, settled`);
  }
  // Commands that only one script starts with keep its name: multi_leg_happy after its three quotes, before the accept.
  const multi = liveOf("multi_leg_happy");
  const r = rebuiltIn(cutAt(multi, 1_000), multi);
  assert.deepEqual([r.name, r.incomplete, r.pass], ["multi_leg_happy", true, null]);
});

await test("a history cut after a run's last command leaves it open; the batches recorded after the cut finish it with its true verdict", () => {
  const happy = liveOf("happy_path");
  for (const ms of [6_000, 7_100]) {
    const fetched = cutAt(happy, ms);
    const { cut, after } = fetched;
    const r = rebuiltIn(fetched, happy);
    // Its last entry alone does not say the history was whole until the reload: then it is not even open.
    assert.equal(S.runsFromHistory(cut).find((x) => x.t0 === happy.t0).pass, null, `+${ms}, by its last entry`);
    // The proposal at +5.2 s was its last command; the contract finalizes at +7.2 s.
    assert.deepEqual([r.name, r.incomplete, r.open, r.pass], ["happy_path", true, true, null], `+${ms}: ${r.detail}`);
    // Before the end of its window (its last wait, +7.3 s, and WAIT_GRACE_MS), it stays open.
    const early = S.settleRun(r, [...cut, ...after.filter((b) => b.time <= happy.t0 + 7_500)], happy.t0 + 7_500);
    assert.deepEqual([early.open, early.pass], [true, null], `+${ms}, at +7.5 s`);
    const done = S.settleRun(r, [...cut, ...after], happy.t0 + 60_000);
    assert.deepEqual([done.name, done.incomplete, done.open, done.pass], ["happy_path", false, false, true], `+${ms}: ${done.detail}`);
    assert.deepEqual(done.own, happy.own, `+${ms}: own`);
    assert.deepEqual(done.batches, happy.batches, `+${ms}: batches`);
    assert.deepEqual(done.labels, happy.labels, `+${ms}: labels`);
  }
});

await test("an open run that gets a command after the history (the live stream carries no action) stays incomplete", () => {
  const happy = liveOf("happy_path");
  const fetched = cutAt(happy, 6_000);
  const { cut, after } = fetched;
  const r = rebuiltIn(fetched, happy);
  assert.equal(r.open, true);
  const unread = { seq: history.at(-1).seq + 1, time: happy.t0 + 6_500, kind: "User", origin: { actor: happy.accounts.carol, nonce: 1 }, events: [], balances: [] };
  const done = S.settleRun(r, [...cut, ...after, unread], happy.t0 + 60_000);
  assert.deepEqual([done.incomplete, done.open, done.pass], [true, false, null], done.detail);
});

await test("a history with nothing but one deposit of a run gets the neutral label, no verdict, and that one batch", () => {
  const t0 = 1_790_000_000_123;
  const alice = t0 * 100 + 10;
  const entries = [
    { seq: 2, time: t0, kind: "User", origin: { actor: 1, nonce: 2 }, events: [{ audience: { Account: alice }, event: { Deposited: { account: alice, amount: 5 } } }], balances: [], command: { actor: 1, nonce: 2, action: { Deposit: { to: alice, amount: 5 } } } },
  ];
  const [run] = S.runsFromHistory(entries);
  assert.equal(run.t0, t0);
  assert.match(run.name, /^run \d\d:\d\d:\d\d$/);
  assert.equal(run.pass, null);
  assert.deepEqual(run.batches, [2]);
  assert.deepEqual(run.own.map((o) => R.eventName(o)), ["Deposited"]);
  assert.deepEqual(S.runsFromHistory([]), []);
});

server.stop();
console.log(failed ? `${failed} FAILED` : "all history tests passed");
process.exit(failed ? 1 : 0);
