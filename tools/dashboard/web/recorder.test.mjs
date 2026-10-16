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

// ---- history: the read API's trace seeded before the live batches -------------------------------------

/** A trace entry of the read API (`GET /v1/trace`): a batch frame without `type`, plus the `command` that made it. */
const entry = (seq, events, { command = null, ...rest } = {}) => {
  const { type, ...b } = batch(seq, events, rest);
  return { ...b, command };
};
const dep = (account, amount) => pub(ev("Deposited", { account, amount }));
const submit = (actor, nonce) => ({
  actor,
  nonce,
  action: { SubmitRequest: { legs: [{ contract: 7, side: "BuyYes", qty: 5, limit_price: 7_000 }], response_deadline: 9 } },
});

test("seedHistory into an empty recorder: batches in seq order, marked history, each with its command", () => {
  const rec = R.createRecorder();
  const cmd = submit(10, 2);
  const entries = [
    entry(1, [pub(ev("Initialized"))], { kind: "Genesis" }),
    entry(2, [dep(10, 5)], { origin: { actor: 1, nonce: 1 }, balances: [bal(10, 5)], command: { actor: 1, nonce: 1, action: { Deposit: { to: 10, amount: 5 } } } }),
    entry(3, [pub(ev("RequestOpened", { request: 3 }))], { origin: { actor: 10, nonce: 2 }, command: cmd }),
  ];
  assert.equal(rec.seedHistory(entries), 3);
  assert.deepEqual(rec.allBatches().map((b) => b.seq), [1, 2, 3]);
  assert.deepEqual(rec.order, [1, 2, 3]);
  assert.deepEqual(rec.batchList().map((b) => b.seq), [1, 2, 3]);
  assert.ok(rec.allBatches().every((b) => b.history === true && b.epoch === 0 && b.chunks === 1));
  // The batch keeps what the entry says: time, kind, origin, events, balances and the command.
  const b = rec.batches.get(3);
  assert.deepEqual([b.time, b.kind, b.origin, b.events, b.balances, b.command], [1_003, "User", { actor: 10, nonce: 2 }, entries[2].events, [], cmd]);
  assert.deepEqual(rec.batches.get(2).balances, [bal(10, 5)]);
  assert.equal(rec.batches.get(1).origin, null);
  assert.equal(rec.batches.get(1).command, null);
  // A copy: the entry's events are not shared with the batch.
  assert.notEqual(b.events, entries[2].events);
  // History is no frame: nothing was received.
  assert.equal(rec.frames.length, 0);
});

test("seedHistory with live batches already there: only the lower seqs are added, and they come before the live ones", () => {
  const rec = R.createRecorder();
  rec.onFrame(batch(7, [dep(10, 1)]));
  rec.onFrame(batch(8, [dep(10, 2)]));
  const added = rec.seedHistory([1, 2, 3, 4, 5, 6, 7, 8, 9].map((s) => entry(s, [dep(10, s)])));
  assert.equal(added, 6, "seqs 7 and up are the live stream's: the live ones stay, 9 is not history's to add");
  assert.deepEqual(rec.allBatches().map((b) => [b.seq, b.history === true]), [[1, true], [2, true], [3, true], [4, true], [5, true], [6, true], [7, false], [8, false]]);
  assert.deepEqual(rec.order, [1, 2, 3, 4, 5, 6, 7, 8]);
  assert.equal(rec.epoch, 0);
  // The live batches are as they came (frames, no command).
  assert.equal(rec.batches.get(7).firstN, 1);
  assert.equal("command" in rec.batches.get(7), false);
  assert.deepEqual(rec.batches.get(8).events, [dep(10, 2)]);
});

test("a live frame whose seq is already history is ignored: no duplicate, no new epoch", () => {
  const rec = R.createRecorder();
  rec.seedHistory([entry(1, [dep(10, 1)]), entry(2, [dep(10, 2)]), entry(3, [dep(10, 3)])]);
  // The WebSocket delivers batches the replica already had (the fetch raced the stream), whole or in chunks.
  assert.equal(rec.onFrame(batch(3, [dep(99, 9)])), null);
  assert.equal(rec.onFrame(batch(2, [dep(99, 9)])), null);
  assert.equal(rec.epoch, 0);
  assert.deepEqual(rec.allBatches().map((b) => b.seq), [1, 2, 3]);
  assert.deepEqual(rec.batches.get(3).events, [dep(10, 3)], "the history batch is untouched");
  assert.equal(rec.batches.get(3).chunks, 1);
  // The frame is still a received frame (the frame log is the wire's), just not a second batch.
  assert.equal(rec.frames.length, 2);
  // The next seq is live as ever: an ordinary batch, after the history, in the same epoch.
  const live = rec.onFrame(batch(4, [dep(10, 4)]));
  assert.deepEqual([live.seq, live.epoch, live.history], [4, 0, undefined]);
  assert.deepEqual(rec.allBatches().map((b) => b.seq), [1, 2, 3, 4]);
  assert.equal(rec.epoch, 0);
});

const seededHistory = () => {
  const rec = R.createRecorder();
  rec.seedHistory([entry(1, [pub(ev("Initialized"))], { kind: "Genesis" }), entry(2, [dep(10, 2)]), entry(3, [dep(10, 3)], { kind: "Tick" })]);
  return rec;
};

test("a live frame with a history batch's seq, time and kind is that batch again: ignored, before and after the first live batch", () => {
  const seeded = seededHistory;
  const same = seeded();
  assert.equal(same.onFrame(batch(2, [dep(10, 2)])), null);
  assert.equal(same.onFrame(batch(3, [dep(10, 3)], { kind: "Tick" })), null);
  assert.equal(same.epoch, 0);
  same.onFrame(batch(4, [dep(10, 4)]));
  assert.equal(same.onFrame(batch(2, [dep(10, 2)])), null, "a late duplicate after the first live batch");
  assert.equal(same.epoch, 0);
  assert.deepEqual(same.allBatches().map((b) => [b.epoch, b.seq, b.history === true]), [[0, 1, true], [0, 2, true], [0, 3, true], [0, 4, false]]);
});

test("a journal reset before the first live batch: a history seq with another time is the new journal's batch, in a new epoch", () => {
  const seeded = seededHistory;
  // `docker compose down -v` after the history loaded, before any live batch. The new journal's batch is
  // its epoch's first, the old history stays as epoch 0, and the driver's lookup by seq finds the new batch.
  const reset = seeded();
  const fresh = reset.onFrame(batch(2, [dep(99, 5)], { time: 9_002, balances: [bal(99, 5)] }));
  assert.equal(reset.epoch, 1);
  assert.deepEqual([fresh.epoch, fresh.seq, fresh.history, fresh.events], [1, 2, undefined, [dep(99, 5)]]);
  assert.equal(reset.batches.get(2), fresh);
  assert.deepEqual(reset.order, [2]);
  assert.deepEqual(reset.batchList().map((b) => b.history), [undefined]);
  assert.deepEqual(reset.allBatches().map((b) => [b.epoch, b.seq, b.history === true]), [[0, 1, true], [0, 2, true], [0, 3, true], [1, 2, false]]);
  // The new journal goes on in its own epoch, a chunk merging with it and its seqs below the old history's top kept apart from it.
  reset.onFrame(batch(2, [dep(99, 6)], { time: 9_002 }));
  assert.equal(reset.batches.get(2).chunks, 2);
  const next = reset.onFrame(batch(3, [dep(99, 7)], { time: 9_003 }));
  assert.deepEqual([reset.epoch, next.epoch, next.history], [1, 1, undefined]);
  assert.deepEqual(reset.order, [2, 3]);
});

test("another kind alone is another batch too, and the new journal's first batch may be the Genesis at seq 1", () => {
  const seeded = seededHistory;
  const kind = seeded();
  kind.onFrame(batch(2, [dep(10, 2)], { kind: "Tick" }));
  assert.equal(kind.epoch, 1);
  const genesis = seeded();
  genesis.onFrame(batch(1, [pub(ev("Initialized"))], { kind: "Genesis", time: 0 }));
  assert.deepEqual([genesis.epoch, genesis.order], [1, [1]]);
});

// The history is the newest seqs of a long journal (the window starts at 5,001), and the stack is reset before
// any live batch reaches the page: the new journal's seqs start far below the history's, at none of its seqs.
const longHistory = () => {
  const rec = R.createRecorder();
  rec.seedHistory([5_001, 5_002, 5_003].map((s) => entry(s, [dep(10, s)])));
  return rec;
};

test("before the first live batch, a seq at or below the history's top that is not that history batch starts a new epoch", () => {
  const rec = longHistory();
  const fresh = rec.onFrame(batch(2, [dep(99, 2)], { time: 9_002 }));
  assert.equal(rec.epoch, 1);
  assert.deepEqual([fresh.epoch, fresh.seq, fresh.history], [1, 2, undefined]);
  assert.deepEqual(rec.order, [2], "the new epoch holds only the new journal's batch");
  assert.deepEqual(rec.allBatches().map((b) => [b.epoch, b.seq, b.history === true]), [[0, 5_001, true], [0, 5_002, true], [0, 5_003, true], [1, 2, false]]);
  // The new journal goes on in its own epoch.
  rec.onFrame(batch(3, [dep(99, 3)], { time: 9_003 }));
  assert.deepEqual([rec.epoch, rec.order], [1, [2, 3]]);
  // The history's top itself, as another batch, is a new journal's too.
  const top = longHistory();
  top.onFrame(batch(5_003, [dep(99, 3)], { time: 9_003 }));
  assert.deepEqual([top.epoch, top.order], [1, [5_003]]);
});

test("before the first live batch, an identical copy of the history's top is still ignored, and a seq above the top is appended", () => {
  const same = longHistory();
  assert.equal(same.onFrame(batch(5_003, [dep(10, 5_003)])), null);
  assert.equal(same.epoch, 0);
  const next = same.onFrame(batch(5_004, [dep(10, 5_004)]));
  assert.deepEqual([same.epoch, next.epoch, next.history], [0, 0, undefined]);
  assert.deepEqual(same.order, [5_001, 5_002, 5_003, 5_004]);
  // Above the top with no copy first: the same epoch, after the history.
  const above = longHistory();
  above.onFrame(batch(5_004, [dep(10, 5_004)]));
  assert.deepEqual([above.epoch, above.order], [0, [5_001, 5_002, 5_003, 5_004]]);
  // With no history at all, the first live batch is just that.
  const empty = R.createRecorder();
  empty.onFrame(batch(2, [dep(10, 2)]));
  assert.deepEqual([empty.epoch, empty.order], [0, [2]]);
});

test("history never sets the newest batch: a live chunk after seeding merges with its live batch; a seq going back is still a new epoch", () => {
  const rec = R.createRecorder();
  rec.seedHistory([entry(1, [dep(10, 1)]), entry(2, [dep(10, 2)])]);
  const a = pub(ev("RequestOpened", { request: 5 }));
  const b = pub(ev("QuoteLost", { quote: 6 }));
  rec.onFrame(batch(5, [a], { origin: { actor: 10, nonce: 3 } }));
  rec.onFrame(batch(5, [b], { origin: { actor: 10, nonce: 3 }, balances: [bal(10, 7)] }));
  assert.equal(rec.batches.get(5).chunks, 2);
  assert.deepEqual(rec.batches.get(5).events, [a, b]);
  assert.deepEqual(rec.allBatches().map((x) => x.seq), [1, 2, 5]);
  // Seeding after that adds the seqs below the live batch (3, 4), not the live one's own.
  assert.equal(rec.seedHistory([entry(3, [dep(10, 3)]), entry(4, [dep(10, 4)]), entry(5, [dep(10, 5)])]), 2);
  assert.deepEqual(rec.allBatches().map((x) => [x.seq, x.history === true]), [[1, true], [2, true], [3, true], [4, true], [5, false]]);
  // A seq below the newest live one that history also has, as another batch, is the journal starting again.
  rec.onFrame(batch(2, [dep(99, 5)], { time: 9_002 }));
  assert.equal(rec.epoch, 1);
  assert.deepEqual(rec.allBatches().map((x) => [x.epoch, x.seq]), [[0, 1], [0, 2], [0, 3], [0, 4], [0, 5], [1, 2]]);
  assert.deepEqual(rec.order, [2]);
  assert.equal(rec.batches.get(2).history, undefined);
});

test("seedHistory twice adds nothing the second time; entries that arrive later but fall before earlier history keep seq order", () => {
  const rec = R.createRecorder();
  const entries = [1, 2, 3].map((s) => entry(s, [dep(10, s)]));
  assert.equal(rec.seedHistory(entries), 3);
  assert.equal(rec.seedHistory(entries), 0);
  assert.deepEqual(rec.allBatches().map((b) => b.seq), [1, 2, 3]);
  assert.equal(rec.seedHistory([]), 0);
  // Older entries seeded after newer ones still come first, ascending.
  const late = R.createRecorder();
  late.seedHistory([entry(5, [dep(10, 5)]), entry(6, [dep(10, 6)])]);
  assert.equal(late.seedHistory([entry(2, [dep(10, 2)]), entry(3, [dep(10, 3)]), entry(5, [dep(10, 5)])]), 2);
  assert.deepEqual(late.allBatches().map((b) => b.seq), [2, 3, 5, 6]);
  assert.deepEqual(late.order, [2, 3, 5, 6]);
  // A seq twice in one call counts once.
  const dup = R.createRecorder();
  assert.equal(dup.seedHistory([entry(1, [dep(10, 1)]), entry(1, [dep(10, 1)])]), 1);
});

test("history belongs to the current epoch, after the older epochs' batches; clear() forgets it", () => {
  const rec = R.createRecorder();
  rec.onFrame(batch(8, [dep(10, 1)]));
  rec.onFrame(batch(2, [dep(10, 2)])); // a new journal: epoch 1
  assert.equal(rec.epoch, 1);
  assert.equal(rec.seedHistory([entry(1, [dep(10, 1)]), entry(2, [dep(10, 9)]), entry(3, [dep(10, 3)])]), 1, "seq 2 is live, 3 is above it");
  assert.deepEqual(rec.allBatches().map((b) => [b.epoch, b.seq, b.history === true]), [[0, 8, false], [1, 1, true], [1, 2, false]]);
  assert.deepEqual(rec.batchList().map((b) => b.seq), [1, 2]);
  rec.clear();
  assert.deepEqual([rec.allBatches().length, rec.order.length, rec.batches.size], [0, 0, 0]);
  assert.equal(rec.seedHistory([entry(1, [dep(10, 1)])]), 1);
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
