// Unit tests of the model (no network): what the panels show at a position of a recording. The
// fixture is happy_path recorded live through rfq-gateway, as the admin connection and as two view
// connections (the run's Alice and MM1) received it. Usage: node web/model.test.mjs
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const R = require("./recorder.js");
const M = require("./model.js");

let failed = 0;
function test(name, fn) {
  try {
    fn();
    console.log(`ok   ${name}`);
  } catch (e) {
    failed++;
    console.log(`FAIL ${name}: ${e.stack?.split("\n").slice(0, 3).join(" | ") ?? e}`);
  }
}

const fx = JSON.parse(readFileSync(new URL("./fixtures/happy_path.live.json", import.meta.url), "utf8"));
const { alice, mm1, mm2, carol, oracle } = fx.run.accounts;
const L = fx.run.labels; // label -> seq: the ids of the run's requests and quotes are these seqs
const contract = fx.run.t0;

/** The page's own commands, by the seq of the batch each made (what the driver's `commandAt` answers). */
const bySeq = new Map(fx.commands.map((c) => [c.seq, c]));
const commandAt = (seq) => bySeq.get(seq);

/** A recording of `frames` as one connection received them: merged by the recorder, as the page does. */
function recording(frames, viewer = "admin", extra = {}) {
  const rec = R.createRecorder();
  for (const f of frames) rec.onFrame(f);
  return { batches: rec.allBatches(), commandAt, viewer, ...extra };
}
const admin = recording(fx.admin);
const indexOf = (rec, seq) => rec.batches.findIndex((b) => b.seq === seq);
const variant = (v) => (typeof v === "string" ? v : Object.keys(v)[0]);
const eventsNamed = (rec, name) => rec.batches.filter((b) => b.events.some((e) => variant(e.event) === name));

test("the fixture is a passing live happy_path", () => {
  assert.equal(fx.run.pass, true);
  assert.equal(fx.admin.length, 13);
  assert.deepEqual(eventsNamed(admin, "RequestPresented").map((b) => b.seq), [14]);
});

test("at the RequestPresented batch: the request is Presented with the engine's picks, its quotes Selected and Lost", () => {
  const at = indexOf(admin, eventsNamed(admin, "RequestPresented")[0].seq);
  const m = M.modelAt(admin, at);
  assert.equal(m.seq, 14);
  assert.equal(m.position, at);
  const r = m.requests.find((x) => x.id === L.req);
  assert.equal(variant(r.state), "Presented");
  const presented = eventsNamed(admin, "RequestPresented")[0].events.find((e) => variant(e.event) === "RequestPresented").event.RequestPresented;
  assert.equal(r.state.Presented.accept_by, presented.accept_by);
  assert.deepEqual(r.picks, presented.picks);
  assert.deepEqual(r.picks.map((p) => p.quote), [L.q2]);
  assert.equal(m.quotes.find((q) => q.id === L.q2).state, "Selected");
  assert.equal(m.quotes.find((q) => q.id === L.q1).state, "Lost");
  assert.equal(r.legs[0].selected, L.q2);
  // The page's own commands fill in what the events don't carry: the requester and her sealed limit,
  // a quote's maker (also its event's audience) and size, the contract's terms.
  assert.equal(r.requester, alice);
  assert.equal(r.legs[0].limit_price, 7_000);
  assert.deepEqual([r.legs[0].side, r.legs[0].qty, r.legs[0].contract], ["BuyYes", 1_000, contract]);
  const q2 = m.quotes.find((q) => q.id === L.q2);
  assert.deepEqual([q2.mm, q2.request, q2.price, q2.size, q2.leg], [mm2, L.req, 6_000, 1_000, 0]);
  const c = m.contracts.find((x) => x.id === contract);
  assert.equal(c.state, "Trading");
  assert.equal(c.description, "Will it rain in Paris on 2026-11-01?");
  assert.deepEqual([c.oracle, c.trading_close, c.resolution_deadline, c.dispute_window], [oracle, contract + 5_000, contract + 15_000, 2_000]);
  // Nothing filled yet.
  assert.deepEqual(m.positions, []);
});

test("balances are the last `balances` seen up to the position; the previous position's are kept for the deltas", () => {
  const lastSeen = (upTo) => {
    const out = new Map();
    for (const b of admin.batches.slice(0, upTo + 1)) for (const { account, balance } of b.balances) out.set(account, balance);
    return out;
  };
  for (let at = 0; at < admin.batches.length; at++) {
    const m = M.modelAt(admin, at);
    const want = lastSeen(at);
    assert.deepEqual(new Map(m.accounts.map((a) => [a.id, a.balance])), want, `at ${at}`);
    assert.deepEqual(m.previous, at === 0 ? new Map() : lastSeen(at - 1), `previous at ${at}`);
  }
  // At the end: exactly the last balances the engine sent for each account, object for object.
  const end = M.modelAt(admin);
  const last20 = admin.batches.at(-1).balances;
  for (const { account, balance } of last20) assert.deepEqual(end.accounts.find((a) => a.id === account).balance, balance);
  assert.deepEqual(end.accounts.map((a) => a.id).sort(), [alice, mm1, mm2, carol].sort());
});

test("the end: request and quote Filled, the contract Final, the position settled with the engine's payouts", () => {
  const m = M.modelAt(admin);
  assert.equal(m.position, admin.batches.length - 1);
  assert.equal(variant(m.requests.find((x) => x.id === L.req).state), "Filled");
  assert.equal(m.quotes.find((q) => q.id === L.q2).state, "Filled");
  const c = m.contracts.find((x) => x.id === contract);
  assert.deepEqual(c.state, { Final: "Yes" });
  assert.deepEqual(c.proposal, { at: admin.batches[indexOf(admin, L.propose)].time, outcome: "Yes", until: c.proposal.until });
  const settled = eventsNamed(admin, "PositionSettled")[0].events.find((e) => variant(e.event) === "PositionSettled").event.PositionSettled;
  assert.equal(m.positions.length, 1);
  const p = m.positions[0];
  assert.deepEqual(
    [p.id, p.contract, p.request, p.qty, p.yes_holder, p.no_holder, p.settled],
    [settled.position, contract, L.req, 1_000, alice, mm2, true],
  );
  assert.deepEqual([p.yes_payout, p.no_payout], [settled.yes_payout, settled.no_payout]);
  // No lock is made up: the events don't carry one.
  assert.deepEqual([p.yes_lock, p.no_lock], [null, null]);
  assert.deepEqual(c.positions, [p.id]);
});

test("the trace: one entry per batch, with kind, actor and nonce from the origin, and the command the page sent", () => {
  const m = M.modelAt(admin);
  assert.equal(m.trace.length, admin.batches.length);
  assert.deepEqual(m.trace.map((e) => e.seq), admin.batches.map((b) => b.seq));
  const req = m.trace.find((e) => e.seq === L.req);
  assert.deepEqual([req.kind, req.actor, req.nonce], ["user", alice, 2]);
  assert.deepEqual(req.command, { User: { actor: alice, nonce: 2, action: commandAt(L.req).action } });
  const tick = m.trace.find((e) => e.seq === 14);
  assert.deepEqual([tick.kind, tick.actor, tick.nonce, tick.command], ["tick", null, null, null]);
  // Balance deltas per row: the balance before this batch, for a display diff.
  const fill = m.trace.find((e) => e.seq === L.accept);
  const a = fill.balances.find((b) => b.account === alice);
  assert.deepEqual(a.before, admin.batches[indexOf(admin, L.req)].balances.find((b) => b.account === alice).balance);
  const first = m.trace.find((e) => e.seq === L.fund_alice);
  assert.equal(first.balances[0].before, null);
  assert.ok(m.trace.every((e) => e.key === `${e.epoch}:${e.seq}`));
});

test("positions: -1 is before the first batch, past the end is the end, and nothing after the position leaks in", () => {
  const empty = M.modelAt(admin, -1);
  assert.deepEqual([empty.accounts, empty.requests, empty.quotes, empty.contracts, empty.trace, empty.seq, empty.now], [[], [], [], [], [], null, null]);
  assert.equal(M.modelAt(admin, 999).position, admin.batches.length - 1);
  const atListing = M.modelAt(admin, indexOf(admin, L.list));
  assert.deepEqual(atListing.requests, []);
  assert.equal(atListing.now, admin.batches[indexOf(admin, L.list)].time);
});

test("names and roles come from the page (run-scoped display names); unknown accounts keep their id", () => {
  const info = new Map([[alice, { name: "Alice (run …3372)", role: "requester" }]]);
  const m = M.modelAt({ ...admin, accountInfo: (id) => info.get(id) });
  const a = m.accounts.find((x) => x.id === alice);
  assert.deepEqual([a.name, a.role], ["Alice (run …3372)", "requester"]);
  const other = m.accounts.find((x) => x.id === mm1);
  assert.deepEqual([other.name, other.role], [`#${mm1}`, "account"]);
});

// ---- account views: only what that connection received ------------------------------------------------

test("as MM1: its own balance, quote and the public request; never the limit, the requester, MM2 or Alice", () => {
  const view = recording(fx.views.mm1, { account: mm1 });
  const m = M.modelAt(view);
  assert.deepEqual(m.accounts.map((a) => a.id), [mm1]);
  const r = m.requests.find((x) => x.id === L.req);
  assert.equal(r.requester, null);
  assert.equal(r.legs[0].limit_price, null);
  assert.deepEqual([r.legs[0].side, r.legs[0].qty], ["BuyYes", 1_000]);
  // MM1 never hears that the request was presented or filled: those go to the requester only.
  assert.equal(variant(r.state), "Open");
  assert.deepEqual(m.quotes.map((q) => [q.id, q.mm, q.size, q.state]), [[L.q1, mm1, 1_000, "Lost"]]);
  // The contract's terms come with the operator's command, which this view never receives.
  const c = m.contracts.find((x) => x.id === contract);
  assert.equal(c.description, null);
  assert.deepEqual(c.state, { Final: "Yes" });
  assert.deepEqual(m.positions, []);
  // The model joins a command only through an origin the gateway forwarded (MM1's own): the page's
  // knowledge of the other commands never reaches this view.
  const text = JSON.stringify(m, (k, v) => (v instanceof Map ? [...v] : v));
  for (const id of [alice, mm2, carol, oracle]) assert.ok(!text.includes(String(id)), `account ${id} in MM1's model`);
  assert.ok(m.requests.every((x) => x.legs.every((leg) => leg.limit_price === null)), "a limit in MM1's model");
  assert.equal(m.trace.find((e) => e.seq === L.fund_mm1).kind, "hidden", "the operator's deposit: origin stripped");
  assert.equal(m.trace.find((e) => e.seq === L.q1).command.User.action.SubmitQuote.price, 6_200);
});

test("as Alice: her request with her limit, presented and filled; quotes sealed; the counterparty named only at settlement", () => {
  const view = recording(fx.views.alice, { account: alice });
  const r14 = M.modelAt(view, indexOf(view, 14));
  const r = r14.requests.find((x) => x.id === L.req);
  assert.deepEqual([r.requester, r.legs[0].limit_price, variant(r.state)], [alice, 7_000, "Presented"]);
  assert.deepEqual(r.picks.map((p) => [p.quote, p.price]), [[L.q2, 6_000]]);
  assert.deepEqual(r14.quotes, [], "quotes are their makers' only");
  const filled = M.modelAt(view, indexOf(view, L.accept));
  assert.equal(filled.positions.length, 1);
  assert.deepEqual([filled.positions[0].yes_holder, filled.positions[0].no_holder], [alice, null]);
  const end = M.modelAt(view);
  assert.deepEqual([end.positions[0].yes_holder, end.positions[0].no_holder, end.positions[0].settled], [alice, mm2, true]);
  assert.deepEqual(end.accounts.map((a) => a.id), [alice]);
  // Before settlement, MM2 appears nowhere in her view.
  const text = JSON.stringify(filled, (k, v) => (v instanceof Map ? [...v] : v));
  for (const id of [mm1, mm2, carol]) assert.ok(!text.includes(String(id)), `account ${id} in Alice's model before settlement`);
});

// ---- hand-written batches: the paths happy_path doesn't take ---------------------------------------

const pub = (event) => ({ audience: "Public", event });
const to = (account, event) => ({ audience: { Account: account }, event });
const batch = (seq, events, { origin = null, balances = [], kind = "User", time = 1_000 + seq } = {}) => ({ type: "batch", seq, time, kind, origin, events, balances });

test("replace, dispute, ruling, a Resume and a rejection: each event moves its object as the engine said", () => {
  const frames = [
    batch(2, [pub({ ContractListed: { contract: 7 } })]),
    batch(3, [pub({ RequestOpened: { request: 3, leg: 0, contract: 7, side: "BuyNo", qty: 5, response_deadline: 9_000 } })]),
    batch(4, [to(20, { QuoteSubmitted: { quote: 4, request: 3, price: 4_000, expires_at: 9_500 } })]),
    batch(5, [to(20, { QuoteReplaced: { old: 4, new: 5 } }), to(20, { QuoteSubmitted: { quote: 5, request: 3, price: 3_900, expires_at: 9_500 } })]),
    batch(6, [to(20, { QuoteLost: { quote: 5 } }), pub({ RequestRejected: { request: 3, cause: "Lapsed" } })], { kind: "Tick" }),
    batch(7, [pub({ OutcomeProposed: { contract: 7, outcome: "No", dispute_until: 12_000 } })]),
    batch(8, [pub({ Disputed: { contract: 7, disputer: 30 } })]),
    batch(9, [], { kind: "Resume" }),
    batch(10, [to(30, { CommandRejected: { reason: "InvalidState" } })], { origin: { actor: 30, nonce: 4 } }),
    batch(11, [pub({ Ruled: { contract: 7, outcome: "Invalid", bond_returned: true } }), pub({ ContractFinalized: { contract: 7, outcome: "Invalid" } })]),
  ];
  const rec = recording(frames);
  const at = (seq) => M.modelAt(rec, indexOf(rec, seq));
  assert.deepEqual(at(5).quotes.map((q) => [q.id, q.state]), [[4, "Replaced"], [5, "Active"]]);
  assert.equal(at(5).quotes.find((q) => q.id === 5).mm, 20, "the maker is the event's audience");
  assert.deepEqual(at(6).requests[0].state, { Rejected: "Lapsed" });
  assert.equal(at(6).requests[0].legs[0].limit_price, null, "no command of the page: no limit");
  assert.deepEqual(at(7).contracts[0].state, { Proposed: { outcome: "No", dispute_until: 12_000 } });
  assert.deepEqual(at(8).contracts[0].state, { Disputed: { proposed: "No" } });
  assert.deepEqual(at(8).contracts[0].dispute, { disputer: 30 });
  assert.equal(at(9).trace.at(-1).kind, "resume");
  assert.deepEqual(at(10).trace.at(-1).events.map((e) => e.event), [{ CommandRejected: { reason: "InvalidState" } }]);
  assert.equal(at(10).trace.at(-1).command, null, "a command the page did not send has no action to show");
  assert.deepEqual(at(11).contracts[0].state, { Final: "Invalid" });
  assert.deepEqual(at(11).contracts[0].ruling, { outcome: "Invalid", bond_returned: true });
});

test("a command joins a batch only when its actor and nonce are the batch's origin", () => {
  const frames = [batch(3, [pub({ RequestOpened: { request: 3, leg: 0, contract: 7, side: "BuyYes", qty: 5, response_deadline: 9 } })], { origin: { actor: 10, nonce: 2 } })];
  const wrong = { actor: 10, nonce: 9, action: { SubmitRequest: { legs: [{ contract: 7, side: "BuyYes", qty: 5, limit_price: 6_000 }], response_deadline: 9 } } };
  const m = M.modelAt({ ...recording(frames), commandAt: () => wrong });
  assert.equal(m.requests[0].requester, null);
  assert.equal(m.requests[0].legs[0].limit_price, null);
  assert.equal(m.trace[0].command, null);
  const right = M.modelAt({ ...recording(frames), commandAt: () => ({ ...wrong, nonce: 2 }) });
  assert.deepEqual([right.requests[0].requester, right.requests[0].legs[0].limit_price], [10, 6_000]);
});

test("a recording that began late, or a batch shared with another run: events about objects it never saw make none", () => {
  const frames = [
    batch(5, [pub({ ContractClosed: { contract: 99 } }), pub({ OutcomeProposed: { contract: 99, outcome: "Yes", dispute_until: 9 } })], { kind: "Tick" }),
    batch(6, [to(10, { PositionSettled: { position: 3, yes_holder: 10, yes_payout: 5, no_holder: 20, no_payout: 0 } }), to(20, { QuoteLost: { quote: 4 } })], { kind: "Tick" }),
    batch(7, [pub({ RequestExpired: { request: 2 } }), pub({ Disputed: { contract: 99, disputer: 30 } }), pub({ ContractFinalized: { contract: 99, outcome: "Yes" } })], { kind: "Tick" }),
  ];
  const m = M.modelAt(recording(frames));
  assert.deepEqual([m.contracts, m.positions, m.requests, m.quotes], [[], [], [], []]);
  assert.equal(m.trace.length, 3, "the batches are still in the trace, as they came");
});

test("a journal reset: batches of two epochs with the same seq are two trace entries, and objects don't mix", () => {
  const frames = [
    batch(3, [pub({ RequestOpened: { request: 3, leg: 0, contract: 7, side: "BuyYes", qty: 5, response_deadline: 9 } })]),
    batch(4, [pub({ RequestExpired: { request: 3 } })], { kind: "Tick" }),
    batch(3, [pub({ RequestOpened: { request: 3, leg: 0, contract: 8, side: "BuyNo", qty: 6, response_deadline: 9 } })]),
  ];
  const m = M.modelAt(recording(frames));
  assert.deepEqual(m.trace.map((e) => e.key), ["0:3", "0:4", "1:3"]);
  // Ids are per journal: the new epoch's request #3 is a new object.
  assert.deepEqual(m.requests.map((r) => [r.epoch, r.id, variant(r.state), r.legs[0].contract]), [[0, 3, "Expired", 7], [1, 3, "Open", 8]]);
});

// ---- last nonces, and linear time ------------------------------------------------------------

test("last nonces: one pass over the batches, per actor, the last consumed (Duplicate and NonceGap consume nothing)", () => {
  const m = M.modelAt(admin);
  assert.equal(m.nonces.get(alice), 3);
  assert.equal(m.nonces.get(1), commandAt(L.list).nonce);
  assert.equal(m.nonces.get(oracle), 2);
  assert.equal(m.nonces.has(mm1), true);
  const early = M.modelAt(admin, indexOf(admin, L.req));
  assert.equal(early.nonces.get(alice), 2, "only up to the position");
  const frames = [
    batch(3, [to(20, { QuoteSubmitted: { quote: 3, request: 1, price: 5, expires_at: 9 } })], { origin: { actor: 20, nonce: 2 } }),
    batch(4, [to(20, { CommandRejected: { reason: "Duplicate" } })], { origin: { actor: 20, nonce: 1 } }),
    batch(5, [to(20, { CommandRejected: { reason: "NonceGap" } })], { origin: { actor: 20, nonce: 9 } }),
    batch(6, [to(20, { CommandRejected: { reason: "InvalidState" } })], { origin: { actor: 20, nonce: 3 } }),
  ];
  assert.equal(M.modelAt(recording(frames), 2).nonces.get(20), 2);
  assert.equal(M.modelAt(recording(frames)).nonces.get(20), 3, "a rejection other than Duplicate/NonceGap consumes its nonce");
});

/**
 * A long session: `runs` happy_path-shaped runs, 12 batches each, on fresh accounts and contracts
 * (`t0 × 100 + id`, as the driver scopes them), with the page's commands for the joins.
 */
function synthetic(runs) {
  const batches = [];
  const commands = new Map();
  let seq = 0;
  const bal = (account, free, reserved = 0, escrowed = 0) => ({ account, balance: { free, reserved, escrowed } });
  for (let k = 0; k < runs; k++) {
    const t0 = 1_800_000_000_000 + k * 20_000;
    const [alice, mm1, mm2, oracle] = [10, 20, 21, 4].map((id) => t0 * 100 + id);
    const add = (make, { origin = null, balances = [], kind = "User", action = null } = {}) => {
      seq++;
      batches.push({ seq, epoch: 0, time: t0 + (seq % 10_000), kind, origin, events: make(seq), balances });
      if (action) commands.set(seq, { actor: origin.actor, nonce: origin.nonce, action });
      return seq;
    };
    for (const [i, acct] of [alice, mm1, mm2].entries()) {
      const n = k * 4 + i + 2;
      add(() => [to(acct, { Deposited: { account: acct, amount: 1_000_000_000 } })], { origin: { actor: 1, nonce: n }, balances: [bal(acct, 1_000_000_000)], action: { Deposit: { to: acct, amount: 1_000_000_000 } } });
    }
    add(() => [pub({ ContractListed: { contract: t0 } })], {
      origin: { actor: 1, nonce: k * 4 + 5 },
      action: { ListContract: { id: t0, description: "Will it rain?", oracle, trading_close: t0 + 5_000, resolution_deadline: t0 + 15_000, dispute_window: 2_000 } },
    });
    const req = add((s) => [pub({ RequestOpened: { request: s, leg: 0, contract: t0, side: "BuyYes", qty: 1_000, response_deadline: t0 + 1_500 } })], {
      origin: { actor: alice, nonce: 2 },
      balances: [bal(alice, 300_000_000, 700_000_000)],
      action: { SubmitRequest: { legs: [{ contract: t0, side: "BuyYes", qty: 1_000, limit_price: 7_000 }], response_deadline: t0 + 1_500 } },
    });
    const quote = (mm, price) => add((s) => [to(mm, { QuoteSubmitted: { quote: s, request: req, price, expires_at: t0 + 10_000 } })], {
      origin: { actor: mm, nonce: 2 },
      balances: [bal(mm, 620_000_000, 380_000_000)],
      action: { SubmitQuote: { request: req, leg: 0, price, size: 1_000, expires_at: t0 + 10_000 } },
    });
    const q1 = quote(mm1, 6_200);
    const q2 = quote(mm2, 6_000);
    add(() => [
      to(mm1, { QuoteLost: { quote: q1 } }),
      to(mm2, { QuoteSelected: { quote: q2 } }),
      to(alice, { RequestPresented: { request: req, picks: [{ leg: 0, quote: q2, price: 6_000 }], accept_by: t0 + 3_500 } }),
    ], { kind: "Tick", balances: [bal(mm1, 1_000_000_000)] });
    add(() => [to(mm2, { QuoteFilled: { quote: q2, position: k + 1 } }), to(alice, { RequestFilled: { request: req, positions: [k + 1] } })], {
      origin: { actor: alice, nonce: 3 },
      balances: [bal(alice, 400_000_000, 0, 600_000_000), bal(mm2, 600_000_000, 0, 400_000_000)],
      action: { Accept: { request: req, picks: [{ leg: 0, quote: q2, price: 6_000 }] } },
    });
    add(() => [pub({ ContractClosed: { contract: t0 } })], { kind: "Tick" });
    add(() => [pub({ OutcomeProposed: { contract: t0, outcome: "Yes", dispute_until: t0 + 7_200 } })], {
      origin: { actor: oracle, nonce: 2 },
      action: { ProposeOutcome: { contract: t0, outcome: "Yes" } },
    });
    add(() => [
      pub({ ContractFinalized: { contract: t0, outcome: "Yes" } }),
      to(alice, { PositionSettled: { position: k + 1, yes_holder: alice, yes_payout: 1_000_000_000, no_holder: mm2, no_payout: 0 } }),
      to(mm2, { PositionSettled: { position: k + 1, yes_holder: alice, yes_payout: 1_000_000_000, no_holder: mm2, no_payout: 0 } }),
    ], { kind: "Tick", balances: [bal(alice, 1_400_000_000), bal(mm2, 600_000_000)] });
  }
  return { batches, commandAt: (s) => commands.get(s), viewer: "admin" };
}

test("linear time: a 26,000-batch session (\"All batches\" after thousands of runs) models well inside a frame budget", () => {
  const runs = 2_167;
  const rec = synthetic(runs);
  assert.ok(rec.batches.length >= 26_000, `${rec.batches.length} batches`);
  const timed = (position) => {
    let best = Infinity;
    let m;
    for (let i = 0; i < 3; i++) {
      const t = performance.now();
      m = M.modelAt(rec, position);
      best = Math.min(best, performance.now() - t);
    }
    return { m, ms: best };
  };
  const end = timed();
  // Generous for a slow CI machine: the quadratic version took seconds here.
  assert.ok(end.ms < 100, `modelAt over ${rec.batches.length} batches took ${end.ms.toFixed(1)} ms`);
  assert.equal(end.m.trace.length, rec.batches.length);
  assert.equal(end.m.requests.length, runs);
  assert.equal(end.m.positions.filter((p) => p.settled).length, runs);
  assert.equal(end.m.accounts.length, runs * 3);
  const mid = timed(13_000);
  assert.ok(mid.ms < 100, `modelAt at the middle took ${mid.ms.toFixed(1)} ms`);
  // The previous position's balances are still exact (the ledger's deltas).
  const last = rec.batches.at(-1);
  for (const { account } of last.balances) assert.notDeepEqual(end.m.previous.get(account), end.m.accounts.find((a) => a.id === account).balance);
  console.log(`     (${rec.batches.length} batches: ${end.ms.toFixed(1)} ms at the end, ${mid.ms.toFixed(1)} ms in the middle)`);
});

console.log(failed ? `${failed} FAILED` : "all model tests passed");
process.exit(failed ? 1 : 0);
