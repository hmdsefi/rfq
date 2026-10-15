// Unit tests of the recorder (no network): chunk merging, balances, percentiles, the scope rule and
// the verdict. Usage: node web/recorder.test.mjs
import assert from "node:assert/strict";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const R = require("./recorder.js");

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

// ---- hand-written frames, in the gateway's shape -----------------------------------------------

const pub = (event) => ({ audience: "Public", event });
const to = (account, event) => ({ audience: { Account: account }, event });
const ev = (name, body) => (body === undefined ? name : { [name]: body });
const batch = (seq, events, { origin = null, balances = [], kind = "User", time = 1_000 + seq } = {}) => ({
  type: "batch",
  seq,
  time,
  kind,
  origin,
  events,
  balances,
});
const bal = (account, free, reserved = 0, escrowed = 0) => ({ account, balance: { free, reserved, escrowed } });
const rejected = (reason) => pub(ev("CommandRejected", { reason }));

// ---- merging split batches ---------------------------------------------------------------------

test("the chunks of a split batch merge by seq, events in arrival order", () => {
  const rec = R.createRecorder();
  const origin = { actor: 10, nonce: 3 };
  const a = pub(ev("RequestOpened", { request: 12 }));
  const b = pub(ev("QuoteSubmitted", { quote: 13 }));
  const c = pub(ev("QuoteLost", { quote: 14 }));
  // The node publishes one batch's chunks one after the other (an older seq after a newer one is a
  // new journal: see the epoch tests).
  rec.onFrame(batch(12, [a, b], { origin }));
  rec.onFrame(batch(12, [c], { origin }));
  rec.onFrame(batch(13, [pub(ev("Deposited", { account: 1, amount: 5 }))]));
  assert.equal(rec.batches.size, 2);
  assert.deepEqual(rec.order, [12, 13]);
  const merged = rec.batches.get(12);
  assert.deepEqual(merged.events, [a, b, c]);
  assert.equal(merged.chunks, 2);
  assert.deepEqual(merged.origin, origin);
  assert.equal(merged.kind, "User");
  assert.equal(rec.batches.get(13).chunks, 1);
  assert.deepEqual(rec.batchList().map((m) => m.seq), [12, 13]);
});

test("balances come from the last chunk, which may carry nothing else", () => {
  const rec = R.createRecorder();
  rec.onFrame(batch(20, [pub(ev("RequestOpened", { request: 20 }))], { balances: [] }));
  rec.onFrame(batch(20, [pub(ev("QuoteLost", { quote: 21 }))], { balances: [] }));
  assert.deepEqual(rec.batches.get(20).balances, []);
  const last = [bal(10, 400, 600, 0), bal(20, 900, 100, 0)];
  rec.onFrame(batch(20, [], { balances: last }));
  const merged = rec.batches.get(20);
  assert.deepEqual(merged.balances, last);
  assert.equal(merged.events.length, 2);
  assert.equal(merged.chunks, 3);
});

test("balances an earlier chunk carried do not survive a later chunk", () => {
  const rec = R.createRecorder();
  rec.onFrame(batch(5, [pub(ev("RequestOpened", { request: 5 }))], { balances: [bal(1, 111)] }));
  rec.onFrame(batch(5, [], { balances: [bal(2, 222)] }));
  assert.deepEqual(rec.batches.get(5).balances, [bal(2, 222)]);
});

test("a batch that is not split is kept as it came", () => {
  const rec = R.createRecorder();
  const frame = batch(7, [pub(ev("Deposited", { account: 10, amount: 9 }))], {
    origin: { actor: 1, nonce: 1 },
    balances: [bal(10, 9)],
  });
  const merged = rec.onFrame(frame);
  assert.equal(merged.chunks, 1);
  assert.deepEqual(merged.events, frame.events);
  assert.deepEqual(merged.balances, frame.balances);
  assert.equal(merged.time, frame.time);
});

// ---- epochs: a journal reset starts the seqs again --------------------------------------------------

test("a seq that goes backwards starts a new epoch: the old batches are kept and never merged with the new", () => {
  const rec = R.createRecorder();
  const old2 = pub(ev("Deposited", { account: 10, amount: 1 }));
  rec.onFrame(batch(1, [pub(ev("Initialized"))]));
  rec.onFrame(batch(2, [old2], { balances: [bal(10, 1)] }));
  rec.onFrame(batch(3, [pub(ev("ContractListed", { contract: 7 }))]));
  assert.equal(rec.epoch, 0);
  // `docker compose down -v`: a fresh journal, seq 2 again, about something else.
  const new2 = pub(ev("Deposited", { account: 99, amount: 5 }));
  rec.onFrame(batch(2, [new2], { balances: [bal(99, 5)] }));
  assert.equal(rec.epoch, 1);
  // The current epoch is what `batches`, `order` and `batchList` (and so the driver's lookups by seq) see.
  assert.deepEqual(rec.batches.get(2).events, [new2]);
  assert.deepEqual(rec.batches.get(2).balances, [bal(99, 5)]);
  assert.equal(rec.batches.get(2).chunks, 1);
  assert.equal(rec.batches.has(3), false);
  assert.deepEqual(rec.order, [2]);
  assert.deepEqual(rec.batchList().map((b) => [b.epoch, b.seq]), [[1, 2]]);
  // Nothing recorded is lost: every batch of every epoch, in arrival order, the old seq 2 as it was.
  assert.deepEqual(rec.allBatches().map((b) => [b.epoch, b.seq]), [[0, 1], [0, 2], [0, 3], [1, 2]]);
  assert.deepEqual(rec.allBatches()[1].events, [old2]);
  assert.deepEqual(rec.allBatches()[1].balances, [bal(10, 1)]);
});

test("a chunk merges only into the newest batch: an older seq seen again is a new epoch, an equal one a chunk", () => {
  const rec = R.createRecorder();
  rec.onFrame(batch(3, [pub(ev("RequestOpened", { request: 3 }))]));
  rec.onFrame(batch(4, [pub(ev("RequestOpened", { request: 4 }))]));
  rec.onFrame(batch(4, [pub(ev("QuoteLost", { quote: 9 }))]));
  assert.equal(rec.batches.get(4).chunks, 2);
  assert.equal(rec.epoch, 0);
  // Seq 3 again, after 4: not a late chunk of 3 (chunks of one batch arrive together), a new journal.
  rec.onFrame(batch(3, [pub(ev("RequestCancelled", { request: 3 }))]));
  assert.equal(rec.epoch, 1);
  assert.equal(rec.allBatches()[0].events.length, 1);
  assert.deepEqual(rec.allBatches().map((b) => [b.epoch, b.seq, b.chunks]), [[0, 3, 1], [0, 4, 2], [1, 3, 1]]);
  // clear() forgets every epoch.
  rec.clear();
  assert.deepEqual([rec.epoch, rec.allBatches().length, rec.batchList().length], [0, 0, 0]);
});

// ---- a view connection: only what the gateway sent that view ----------------------------------------

/** A stand-in for an `RfqGateway` connection: `fire(type, payload)` calls what `on` registered. */
function fakeConnection() {
  const listeners = new Map();
  return {
    on(type, fn) {
      if (!listeners.has(type)) listeners.set(type, new Set());
      listeners.get(type).add(fn);
      return () => listeners.get(type).delete(fn);
    },
    fire(type, payload) {
      for (const t of type === "conn" ? ["conn"] : [type, "*"]) for (const fn of listeners.get(t) ?? []) fn(payload, {});
    },
    frame(f) {
      this.fire(f.type, f);
    },
    count: () => [...listeners.values()].reduce((n, s) => n + s.size, 0),
  };
}

test("attachViews: frames go to the recorder of the view the last welcome confirmed; none before the first welcome", () => {
  const gw = fakeConnection();
  const recs = { 10: R.createRecorder(), 20: R.createRecorder() };
  const asked = [];
  const off = R.attachViews(gw, (view) => {
    asked.push(view);
    return view === "admin" ? null : recs[view.account] ?? null;
  });
  // Until its first hello a connection has the admin view: what it gets then is never a participant's.
  gw.frame(batch(1, [pub(ev("Deposited", { account: 21, amount: 5 }))], { balances: [bal(21, 5)] }));
  assert.equal(recs[10].frames.length + recs[20].frames.length, 0);
  gw.frame({ type: "welcome", view: { account: 20 }, engine: "up", now: 1 });
  gw.frame(batch(2, [to(20, ev("QuoteSubmitted", { quote: 2 }))], { balances: [bal(20, 9)] }));
  assert.deepEqual(recs[20].batchList().map((b) => b.seq), [2]);
  assert.equal(recs[20].statuses.length, 1, "the welcome itself is recorded for its view");
  // The view switches: what follows the new welcome belongs to the new view.
  gw.frame({ type: "welcome", view: { account: 10 }, engine: "up", now: 2 });
  gw.frame(batch(3, [to(10, ev("RequestPresented", { request: 1, picks: [], accept_by: 5 }))]));
  assert.deepEqual(recs[10].batchList().map((b) => b.seq), [3]);
  assert.deepEqual(recs[20].batchList().map((b) => b.seq), [2]);
  // A dropped connection: the next socket starts in the admin view again, so nothing until a welcome.
  gw.fire("conn", { up: false });
  gw.frame(batch(4, [pub(ev("Deposited", { account: 21, amount: 5 }))], { balances: [bal(21, 5)] }));
  assert.equal(recs[10].batches.has(4) || recs[20].batches.has(4), false);
  gw.frame({ type: "welcome", view: { account: 10 }, engine: "up", now: 3 });
  gw.frame(batch(5, [pub(ev("ContractClosed", { contract: 1 }))]));
  assert.deepEqual(recs[10].batchList().map((b) => b.seq), [3, 5]);
  // An admin welcome has no recorder here: its frames are dropped.
  gw.frame({ type: "welcome", view: "admin", engine: "up", now: 4 });
  gw.frame(batch(6, [pub(ev("ContractClosed", { contract: 2 }))]));
  assert.equal(recs[10].batches.has(6), false);
  assert.ok(asked.some((v) => v === "admin"));
  off();
  assert.equal(gw.count(), 0, "off() removes every listener");
});

// ---- what the recorder keeps -----------------------------------------------------------------------

test("every frame gets the receive time, and results, statuses and errors are kept", () => {
  const rec = R.createRecorder();
  const before = Date.now();
  rec.onFrame({ type: "welcome", view: "admin", engine: "up", now: 5 });
  rec.onFrame({ type: "sent", id: "c1", actor: 10, nonce: 1 });
  rec.onFrame(batch(1, []), { n: 3, at: 42, t: 7.5 });
  rec.onFrame({ type: "result", id: "c1", seq: 1, latency_us: 840 });
  rec.onFrame({ type: "status", engine: "down", last_seq: 1 });
  rec.onFrame({ type: "error", id: "c2", error: "engine unavailable" });
  rec.onFrame({ type: "error", id: null, error: "bad request: x" });
  assert.equal(rec.frames.length, 7);
  assert.ok(rec.frames.every((f) => typeof f.at === "number" && typeof f.t === "number" && f.n > 0));
  assert.ok(rec.frames[0].at >= before);
  assert.deepEqual([rec.frames[2].n, rec.frames[2].at, rec.frames[2].t], [3, 42, 7.5]);
  assert.deepEqual(rec.results.map((r) => [r.id, r.seq, r.latency_us]), [["c1", 1, 840]]);
  assert.deepEqual(rec.statuses.map((s) => [s.kind, s.engine, s.last_seq]), [["welcome", "up", null], ["status", "down", 1]]);
  assert.deepEqual(rec.errors.map((e) => [e.id, e.error]), [["c2", "engine unavailable"], [null, "bad request: x"]]);
  rec.clear();
  assert.equal(rec.frames.length + rec.batches.size + rec.results.length + rec.statuses.length + rec.errors.length, 0);
});

test("attach feeds the recorder from a gateway's `*` listener", () => {
  const rec = R.createRecorder();
  let listener;
  const off = rec.attach({ on: (type, fn) => ((listener = [type, fn]), () => {}) });
  assert.equal(listener[0], "*");
  listener[1]({ type: "status", engine: "up", last_seq: null }, { n: 1, at: 1, t: 1 });
  assert.equal(rec.statuses.length, 1);
  assert.equal(typeof off, "function");
});

// ---- percentiles -----------------------------------------------------------------------------------

test("percentiles are nearest-rank on known samples", () => {
  const one20 = Array.from({ length: 20 }, (_, i) => i + 1);
  assert.deepEqual(R.summarize(one20), { n: 20, p50: 10, p95: 19, max: 20 });
  const one100 = Array.from({ length: 100 }, (_, i) => i + 1);
  assert.deepEqual(R.summarize(one100), { n: 100, p50: 50, p95: 95, max: 100 });
  assert.deepEqual(R.summarize([7]), { n: 1, p50: 7, p95: 7, max: 7 });
  assert.deepEqual(R.summarize([5, 1]), { n: 2, p50: 1, p95: 5, max: 5 });
  assert.deepEqual(R.summarize([]), { n: 0, p50: null, p95: null, max: null });
});

test("percentiles do not depend on the order of the samples, and use the numbers, not the text", () => {
  assert.deepEqual(R.summarize([30, 2, 100, 11, 5]), { n: 5, p50: 11, p95: 100, max: 100 });
  const shuffled = [9, 2, 11, 1, 10, 3, 8, 4, 7, 5, 6, 12, 20, 15, 13, 14, 19, 16, 18, 17];
  assert.deepEqual(R.summarize(shuffled), { n: 20, p50: 10, p95: 19, max: 20 });
  assert.equal(R.percentile([1, 2, 3, 4], 0), 1);
  assert.equal(R.percentile([1, 2, 3, 4], 100), 4);
  assert.equal(R.percentile([1, 2, 3, 4], 75), 3);
});

test("a run's perf: latency and round trip as p50 / p95 / max, commands per second over the wall time", () => {
  const commands = [
    { latency_us: 800, rtt_ms: 1.5 },
    { latency_us: 900, rtt_ms: 2.5 },
    { latency_us: 4_000, rtt_ms: 9 },
    { latency_us: 700, rtt_ms: 1 },
  ];
  const perf = R.runPerf(commands, 2_000);
  assert.deepEqual(perf.latency_us, { n: 4, p50: 800, p95: 4_000, max: 4_000 });
  assert.deepEqual(perf.rtt_ms, { n: 4, p50: 1.5, p95: 9, max: 9 });
  assert.equal(perf.commands, 4);
  assert.equal(perf.wall_ms, 2_000);
  assert.equal(perf.commands_per_s, 2);
  assert.equal(R.runPerf([], 0).commands_per_s, null);
});

test("the aggregate counts every command of every run once, over the whole window", () => {
  const runs = [
    { commands: [{ latency_us: 100, rtt_ms: 1 }, { latency_us: 300, rtt_ms: 3 }] },
    { commands: [{ latency_us: 200, rtt_ms: 2 }] },
  ];
  const agg = R.aggregatePerf(runs, 1_500);
  assert.equal(agg.runs, 2);
  assert.equal(agg.commands, 3);
  assert.deepEqual(agg.latency_us, { n: 3, p50: 200, p95: 300, max: 300 });
  assert.equal(agg.commands_per_s, 2);
});

// ---- the verdict -------------------------------------------------------------------------------------

test("containsInOrder: order matters, other events may be interleaved, a rejection can pin its reason", () => {
  const events = [
    pub(ev("RequestOpened", { request: 1 })),
    to(20, ev("QuoteSubmitted", { quote: 2 })),
    rejected("InvalidState"),
    pub(ev("RequestFilled", { request: 1 })),
  ];
  assert.deepEqual(R.containsInOrder(events, ["RequestOpened", "RequestFilled"]), { ok: true, error: null });
  assert.deepEqual(R.containsInOrder(events, []), { ok: true, error: null });
  assert.equal(R.containsInOrder(events, ["RequestOpened", "CommandRejected", "RequestFilled"]).ok, true);
  assert.equal(R.containsInOrder(events, ["CommandRejected(InvalidState)"]).ok, true);
  const wrongReason = R.containsInOrder(events, ["CommandRejected(Unauthorized)"]);
  assert.equal(wrongReason.ok, false);
  assert.match(wrongReason.error, /expected event CommandRejected\(Unauthorized\)/);
  const wrongOrder = R.containsInOrder(events, ["RequestFilled", "RequestOpened"]);
  assert.equal(wrongOrder.ok, false);
  assert.match(wrongOrder.error, /expected event RequestOpened \(in order \[RequestFilled, RequestOpened\]\)/);
  // One event satisfies one expectation, not two.
  assert.equal(R.containsInOrder(events, ["QuoteSubmitted", "QuoteSubmitted"]).ok, false);
  // A reason can only be pinned on a rejection.
  assert.equal(R.containsInOrder([pub(ev("RequestFilled", { request: 1 }))], ["RequestFilled(InvalidState)"]).ok, false);
});

// ---- scope -----------------------------------------------------------------------------------------

test("accounts are per run, and the genesis roles are shared", () => {
  const a = new R.Scope(1_000);
  const b = new R.Scope(2_000);
  assert.equal(a.account(10), 100_010);
  assert.notEqual(a.account(10), b.account(10));
  assert.notEqual(a.account(4), b.account(4));
  for (const role of [1, 2, 3]) assert.equal(a.account(role), role);
  assert.throws(() => a.account(100), /no room in a run/);
  assert.ok(!a.accounts.has(1));
  assert.ok(a.accounts.has(100_010));
});

test("another run's events are not owned (rfq-demo scope.rs, on hand-written batches)", () => {
  const scope = new R.Scope(1_000);
  const alice = scope.account(10);
  scope.sent(alice, 1);
  const ours = batch(7, [], { origin: { actor: alice, nonce: 1 } });
  const tick = batch(8, [], { kind: "Tick" });
  const theirs = batch(9, [], { origin: { actor: 42, nonce: 1 } });
  const invalid = rejected("InvalidState");
  assert.equal(scope.owns(ours, invalid), true);
  assert.equal(scope.owns(theirs, invalid), false);
  // A timer never rejects a run's command.
  assert.equal(scope.owns(tick, invalid), false);
  // Request 7 was made by our command (seq 7); request 9 by someone else's.
  assert.equal(scope.owns(tick, pub(ev("RequestExpired", { request: 7 }))), true);
  assert.equal(scope.owns(tick, pub(ev("RequestExpired", { request: 9 }))), false);
  const finalized = (c) => pub(ev("ContractFinalized", { contract: c, outcome: "Yes" }));
  assert.equal(scope.owns(tick, finalized(1_000)), true);
  assert.equal(scope.owns(tick, finalized(2_000)), false);
  assert.equal(scope.owns(tick, pub("Initialized")), false);
  assert.equal(scope.unclassified.size, 0);
});

test("a nonce is part of an origin: the same actor's other command is not ours until it is sent", () => {
  const scope = new R.Scope(1_000);
  const mm = scope.account(20);
  scope.sent(mm, 1);
  assert.equal(scope.owns(batch(5, [], { origin: { actor: mm, nonce: 2 } }), rejected("NonceGap")), false);
  scope.sent(mm, 2);
  assert.equal(scope.owns(batch(6, [], { origin: { actor: mm, nonce: 2 } }), rejected("NonceGap")), true);
});

test("the objects of a run are the seqs of its own commands: quotes, requests, replaced quotes", () => {
  const scope = new R.Scope(5_000);
  const alice = scope.account(10);
  const mm1 = scope.account(20);
  scope.sent(alice, 1);
  scope.sent(mm1, 1);
  scope.owns(batch(30, [], { origin: { actor: alice, nonce: 1 } }), pub("Initialized"));
  scope.owns(batch(31, [], { origin: { actor: mm1, nonce: 1 } }), pub("Initialized"));
  const t = batch(40, [], { kind: "Tick" });
  assert.equal(scope.owns(t, pub(ev("RequestOpened", { request: 30 }))), true);
  assert.equal(scope.owns(t, to(mm1, ev("QuoteSubmitted", { quote: 31 }))), true);
  assert.equal(scope.owns(t, to(mm1, ev("QuoteSubmitted", { quote: 32 }))), false);
  assert.equal(scope.owns(t, to(mm1, ev("QuoteReplaced", { old: 99, new: 31 }))), true);
  assert.equal(scope.owns(t, to(mm1, ev("QuoteReplaced", { old: 31, new: 99 }))), true);
  assert.equal(scope.owns(t, to(mm1, ev("QuoteReplaced", { old: 98, new: 99 }))), false);
  assert.equal(scope.owns(t, pub(ev("Deposited", { account: alice, amount: 1 }))), true);
  assert.equal(scope.owns(t, pub(ev("Deposited", { account: 10, amount: 1 }))), false);
  assert.equal(scope.owns(t, pub(ev("Withdrawn", { account: mm1, amount: 1 }))), true);
  const settled = (yes, no) => pub(ev("PositionSettled", { position: 1, yes_holder: yes, yes_payout: 1, no_holder: no, no_payout: 1 }));
  assert.equal(scope.owns(t, settled(alice, 77)), true);
  assert.equal(scope.owns(t, settled(77, mm1)), true);
  assert.equal(scope.owns(t, settled(77, 78)), false);
});

test("a run's extra markets are owned too: t0 * 10 + k for k in 1..=9, no other (rfq-demo scope.rs)", () => {
  const scope = new R.Scope(1_000);
  const tick = batch(5, [], { kind: "Tick" });
  const finalized = (c) => pub(ev("ContractFinalized", { contract: c, outcome: "Yes" }));
  const listed = (c) => pub(ev("ContractListed", { contract: c }));
  assert.equal(scope.owns(tick, finalized(1_000)), true, "the run's own contract");
  for (let k = 1; k <= 9; k++) {
    assert.equal(scope.owns(tick, finalized(10_000 + k)), true, `market ${k}`);
    assert.equal(scope.owns(tick, listed(10_000 + k)), true, `market ${k} listed`);
  }
  assert.equal(scope.owns(tick, finalized(10_000)), false, "t0 * 10 is not a market");
  assert.equal(scope.owns(tick, finalized(10_010)), false, "k = 10 is not a market");
  assert.equal(scope.owns(tick, finalized(20_001)), false, "another run's market");
  assert.equal(scope.owns(tick, finalized(999)), false);
  // A key whose times 10 is not a safe integer owns only its own contract (the Rust scope's checked_mul).
  const huge = new R.Scope(Number.MAX_SAFE_INTEGER);
  assert.equal(huge.owns(tick, finalized(1)), false);
  assert.equal(huge.owns(tick, finalized(Number.MAX_SAFE_INTEGER)), true);
  assert.equal(scope.unclassified.size, 0);
});

test("two runs' markets stay apart: each market id is owned by exactly one run", () => {
  const a = new R.Scope(1_800_000_000_000);
  const b = new R.Scope(1_800_000_000_001); // one ms later: its markets are 10 ids further on
  const tick = batch(5, [], { kind: "Tick" });
  for (let k = 1; k <= 9; k++) {
    const mine = pub(ev("ContractListed", { contract: 1_800_000_000_000 * 10 + k }));
    assert.deepEqual([a.owns(tick, mine), b.owns(tick, mine)], [true, false], `market ${k} of run a`);
    const theirs = pub(ev("ContractListed", { contract: 1_800_000_000_001 * 10 + k }));
    assert.deepEqual([a.owns(tick, theirs), b.owns(tick, theirs)], [false, true], `market ${k} of run b`);
  }
});

test("every event the engine can emit has a rule (none is unclassified)", () => {
  const names = [
    "Initialized", "CommandRejected", "Deposited", "Withdrawn", "ContractListed", "ContractClosed",
    "RequestOpened", "RequestCancelled", "RequestExpired", "RequestPresented", "RequestRejected",
    "RequestFilled", "QuoteSubmitted", "QuoteReplaced", "QuoteCancelled", "QuoteExpired", "QuoteLost",
    "QuoteSelected", "QuoteReleased", "QuoteFilled", "OutcomeProposed", "Disputed", "Ruled",
    "ContractFinalized", "PositionSettled", "DisputeBondReturned",
  ];
  assert.equal(names.length, 26);
  const scope = new R.Scope(1_000);
  for (const name of names) {
    // The engine's own fields; every one of them is read by the rule for its event.
    const body = name === "Initialized" ? undefined : { reason: "Duplicate", account: 1, contract: 1, request: 1, quote: 1, old: 1, new: 2, yes_holder: 1, no_holder: 2 };
    scope.owns(batch(1, []), pub(ev(name, body)));
  }
  assert.equal(scope.unclassified.size, 0);
  scope.owns(batch(1, []), pub(ev("SomethingNew", {})));
  assert.deepEqual([...scope.unclassified], ["SomethingNew"]);
});

test("two runs partition a shared stream: every event is owned by exactly one run", () => {
  const a = new R.Scope(1_000);
  const b = new R.Scope(2_000);
  const [aliceA, aliceB] = [a.account(10), b.account(10)];
  const [mmA, mmB] = [a.account(20), b.account(20)];
  a.sent(aliceA, 1);
  b.sent(aliceB, 1);
  a.sent(mmA, 1);
  b.sent(mmB, 1);
  const stream = [
    batch(1, [pub(ev("Deposited", { account: aliceA, amount: 5 }))], { origin: { actor: 1, nonce: 1 } }),
    batch(2, [pub(ev("RequestOpened", { request: 2 }))], { origin: { actor: aliceA, nonce: 1 } }),
    batch(3, [pub(ev("RequestOpened", { request: 3 }))], { origin: { actor: aliceB, nonce: 1 } }),
    batch(4, [to(mmB, ev("QuoteSubmitted", { quote: 4 }))], { origin: { actor: mmB, nonce: 1 } }),
    batch(5, [to(mmA, ev("QuoteSubmitted", { quote: 5 }))], { origin: { actor: mmA, nonce: 1 } }),
    batch(6, [pub(ev("RequestExpired", { request: 3 })), pub(ev("RequestExpired", { request: 2 })), pub(ev("QuoteLost", { quote: 5 }))], { kind: "Tick" }),
    batch(7, [pub(ev("ContractClosed", { contract: 2_000 })), pub(ev("ContractClosed", { contract: 1_000 }))], { kind: "Tick" }),
    batch(8, [rejected("Duplicate")], { origin: { actor: mmB, nonce: 1 } }),
  ];
  const owners = [];
  for (const f of stream) for (const e of f.events) owners.push([a.owns(f, e), b.owns(f, e)]);
  assert.ok(owners.every(([x, y]) => x !== y), JSON.stringify(owners));
  assert.deepEqual(owners.map(([x]) => x), [true, true, false, false, true, false, true, true, false, true, false]);
});

console.log(failed ? `${failed} FAILED` : "all recorder tests passed");
process.exit(failed ? 1 : 0);
