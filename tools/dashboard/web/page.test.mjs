// DOM-level test of the live page (no browser, no network): the page's own script from index.html and
// the four scripts it inlines run in a vm context against a stub DOM and a fake gateway that filters
// each connection's frames by its view, as gateway.md §4 says. A run FAILs; the account view's rendered
// output must hold nothing driver-sourced and nothing of another participant. Usage: node web/page.test.mjs
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

const here = (f) => new URL(f, import.meta.url);
const source = (f) => readFileSync(here(f), "utf8");

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
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// ---- a stub DOM: enough for the page's renderers (innerHTML strings, a keyed child list for the log) ----

const stripTags = (h) => h.replace(/<[^>]*>/g, "");
class El {
  constructor(id = null, tag = "div") {
    this.id = id;
    this.tagName = tag.toUpperCase();
    this.ownHtml = "";
    this.kids = [];
    this.parent = null;
    this.dataset = {};
    this.style = { setProperty() {} };
    this.attrs = {};
    this.className = "";
    this.value = "";
    this.disabled = false;
    this.hidden = false;
    this.title = "";
    this.scrollTop = 0;
    this.scrollHeight = 0;
    this.clientHeight = 0;
    this.offsetHeight = 40;
  }
  set innerHTML(h) {
    this.ownHtml = String(h);
    for (const k of this.kids) k.parent = null;
    this.kids = [];
  }
  get innerHTML() {
    return this.ownHtml + this.kids.map((k) => k.innerHTML).join("");
  }
  set textContent(t) {
    this.innerHTML = String(t);
  }
  get textContent() {
    return stripTags(this.innerHTML);
  }
  get children() {
    return this.kids;
  }
  get firstElementChild() {
    return this.kids[0] ?? null;
  }
  get nextElementSibling() {
    if (!this.parent) return null;
    return this.parent.kids[this.parent.kids.indexOf(this) + 1] ?? null;
  }
  insertBefore(node, ref) {
    node.remove();
    const at = ref ? this.kids.indexOf(ref) : -1;
    if (at < 0) this.kids.push(node);
    else this.kids.splice(at, 0, node);
    node.parent = this;
    return node;
  }
  replaceWith(node) {
    if (!this.parent) return;
    node.remove();
    this.parent.kids[this.parent.kids.indexOf(this)] = node;
    node.parent = this.parent;
    this.parent = null;
  }
  remove() {
    if (!this.parent) return;
    this.parent.kids.splice(this.parent.kids.indexOf(this), 1);
    this.parent = null;
  }
  setAttribute(k, v) {
    this.attrs[k] = String(v);
  }
  getAttribute(k) {
    return this.attrs[k] ?? null;
  }
  addEventListener() {}
  focus() {}
  scrollIntoView() {}
  closest() {
    return null;
  }
  contains() {
    return true;
  }
  querySelector() {
    return null;
  }
  querySelectorAll() {
    return [];
  }
}

function stubDocument() {
  const byId = new Map();
  const body = new El("body");
  const doc = {
    byId,
    hidden: false,
    title: "",
    activeElement: body,
    body,
    documentElement: new El("html"),
    getElementById(id) {
      if (!byId.has(id)) byId.set(id, new El(id));
      return byId.get(id);
    },
    createElement: (tag) => new El(null, tag),
    createRange: () => ({ selectNodeContents() {} }),
    contains: () => true,
    querySelector: () => null,
    querySelectorAll: () => [],
    addEventListener() {},
    dispatchEvent() {},
  };
  return doc;
}

// ---- a fake gateway: hello/welcome, the view filter, scripted answers --------------------------------

/** What gateway.md §4 lets an account view have of a batch (null: nothing, the batch is dropped). */
function filterFor(view, frame) {
  if (view === "admin") return frame;
  const me = view.account;
  const events = frame.events.filter((e) => e.audience === "Public" || e.audience?.Account === me);
  const balances = frame.balances.filter((b) => b.account === me);
  if (!events.length && !balances.length) return null;
  return { ...frame, events, balances, origin: frame.origin?.actor === me ? frame.origin : null };
}

function fakeGateway() {
  const server = { sockets: [], seq: 100, nonces: new Map(), commands: [] };
  const deliver = (socket, frame) => socket.onmessage?.({ data: JSON.stringify(frame) });
  const broadcast = (frame) => {
    for (const s of server.sockets) {
      if (s.closed) continue;
      const out = filterFor(s.view, frame);
      if (out) deliver(s, out);
    }
  };
  // A multi-leg engine, enough for the two multi-leg scenarios (`server.sim = true`): the holds, the
  // selection at the deadline (all legs or none), the accept, and the batches with the balances.
  const sim = { requests: new Map(), balances: new Map(), position: 0 };
  const bal = (a) => sim.balances.get(a) ?? { free: 0, reserved: 0, escrowed: 0 };
  const to = (account, event) => ({ audience: { Account: account }, event });
  const pub = (event) => ({ audience: "Public", event });
  /** Moves `by` between a balance's buckets; returns the account, so the batch carries its new balance. */
  function move(account, { free = 0, reserved = 0, escrowed = 0 }) {
    const b = bal(account);
    sim.balances.set(account, { free: b.free + free, reserved: b.reserved + reserved, escrowed: b.escrowed + escrowed });
    return account;
  }
  const batchOf = (seq, kind, origin, events, touched) =>
    ({ type: "batch", seq, time: Date.now(), kind, origin, events, balances: [...new Set(touched)].map((account) => ({ account, balance: bal(account) })) });
  /** The response deadline fires: every leg needs an eligible quote (at or under the limit), else the request expires whole. */
  function deadline(id) {
    const r = sim.requests.get(id);
    const events = [];
    const touched = [];
    const best = r.legs.map((l, leg) => r.quotes.filter((q) => q.leg === leg && q.price <= l.limit_price).sort((x, y) => x.price - y.price)[0]);
    const release = (q) => touched.push(move(q.mm, { free: q.hold, reserved: -q.hold }));
    if (best.every(Boolean)) {
      for (const q of r.quotes.filter((x) => !best.includes(x))) {
        events.push(to(q.mm, { QuoteLost: { quote: q.id } }));
        release(q);
      }
      for (const q of best) events.push(to(q.mm, { QuoteSelected: { quote: q.id } }));
      const picks = best.map((q, leg) => ({ leg, quote: q.id, price: q.price }));
      events.push(to(r.requester, { RequestPresented: { request: id, picks, accept_by: Date.now() + 2_000 } }));
      r.best = best;
    } else {
      for (const q of r.quotes) {
        events.push(to(q.mm, { QuoteLost: { quote: q.id } }));
        release(q);
      }
      events.push(pub({ RequestExpired: { request: id } }));
      touched.push(move(r.requester, { free: r.hold, reserved: -r.hold }));
    }
    broadcast(batchOf(++server.seq, "Tick", null, events, touched));
  }
  function simulate(msg, socket) {
    const [kind, a] = Object.entries(msg.action)[0];
    const nonce = msg.nonce ?? (server.nonces.get(msg.actor) ?? 1) + 1;
    server.nonces.set(msg.actor, nonce);
    deliver(socket, { type: "sent", id: msg.id, actor: msg.actor, nonce });
    const seq = ++server.seq;
    const events = [];
    const touched = [];
    switch (kind) {
      case "Deposit":
        events.push(to(a.to, { Deposited: { account: a.to, amount: a.amount } }));
        touched.push(move(a.to, { free: a.amount }));
        break;
      case "ListContract":
        events.push(pub({ ContractListed: { contract: a.id } }));
        break;
      case "SubmitRequest": {
        a.legs.forEach((l, leg) => events.push(pub({ RequestOpened: { request: seq, leg, contract: l.contract, side: l.side, qty: l.qty, response_deadline: a.response_deadline } })));
        const hold = a.legs.reduce((total, l) => total + l.qty * l.limit_price * 100, 0);
        touched.push(move(msg.actor, { free: -hold, reserved: hold }));
        sim.requests.set(seq, { requester: msg.actor, legs: a.legs, hold, quotes: [], best: null });
        setTimeout(() => deadline(seq), Math.max(0, a.response_deadline - Date.now()));
        break;
      }
      case "SubmitQuote": {
        const r = sim.requests.get(a.request);
        const hold = r.legs[a.leg].qty * (10_000 - a.price) * 100;
        r.quotes.push({ id: seq, mm: msg.actor, leg: a.leg, price: a.price, hold });
        events.push(to(msg.actor, { QuoteSubmitted: { quote: seq, request: a.request, price: a.price, expires_at: a.expires_at } }));
        touched.push(move(msg.actor, { free: -hold, reserved: hold }));
        break;
      }
      case "Accept": {
        const r = sim.requests.get(a.request);
        const positions = [];
        let total = 0;
        r.best.forEach((q, leg) => {
          positions.push(++sim.position);
          total += r.legs[leg].qty * q.price * 100;
          events.push(to(q.mm, { QuoteFilled: { quote: q.id, position: sim.position } }));
          touched.push(move(q.mm, { reserved: -q.hold, escrowed: q.hold }));
        });
        events.push(to(r.requester, { RequestFilled: { request: a.request, positions } }));
        touched.push(move(r.requester, { free: r.hold - total, reserved: -r.hold, escrowed: total }));
        break;
      }
      default:
        throw new Error(`the simulated engine does not know ${kind}`);
    }
    broadcast(batchOf(seq, "User", { actor: msg.actor, nonce }, events, touched));
    deliver(socket, { type: "result", id: msg.id, seq, latency_us: 1234 });
  }

  // The operator's setup goes through; Alice's request finds the engine down: the run FAILs on it.
  function answer(msg, socket) {
    server.commands.push(msg);
    if (server.sim) return simulate(msg, socket);
    const [kind, body] = Object.entries(msg.action)[0];
    if (kind !== "Deposit" && kind !== "ListContract") {
      deliver(socket, { type: "error", id: msg.id, error: "engine unavailable" });
      return;
    }
    const nonce = msg.nonce ?? (server.nonces.get(msg.actor) ?? 1) + 1;
    server.nonces.set(msg.actor, nonce);
    deliver(socket, { type: "sent", id: msg.id, actor: msg.actor, nonce });
    const seq = ++server.seq;
    const events = kind === "Deposit"
      ? [{ audience: { Account: body.to }, event: { Deposited: { account: body.to, amount: body.amount } } }]
      : [{ audience: "Public", event: { ContractListed: { contract: body.id } } }];
    const balances = kind === "Deposit" ? [{ account: body.to, balance: { free: body.amount, reserved: 0, escrowed: 0 } }] : [];
    broadcast({ type: "batch", seq, time: Date.now(), kind: "User", origin: { actor: msg.actor, nonce }, events, balances });
    deliver(socket, { type: "result", id: msg.id, seq, latency_us: 1234 });
    if (kind === "ListContract") {
      // Someone else's public event that names an account (Disputed names its disputer): every view
      // gets the id. Here Carol of the same run, from a command that is not the page's.
      const carol = body.id * 100 + 30;
      broadcast({ type: "batch", seq: ++server.seq, time: Date.now(), kind: "User", origin: { actor: carol, nonce: 9 },
        events: [{ audience: "Public", event: { Disputed: { contract: body.id, disputer: carol } } }], balances: [] });
    }
  }
  server.Socket = class {
    constructor(url) {
      this.url = url;
      this.view = "admin";
      this.closed = false;
      server.sockets.push(this);
      setTimeout(() => this.onopen?.({}), 0);
    }
    send(text) {
      const msg = JSON.parse(text);
      setTimeout(() => {
        if (this.closed) return;
        if (msg.type === "hello") {
          this.view = msg.view;
          deliver(this, { type: "welcome", view: msg.view, engine: "up", now: Date.now() });
          deliver(this, { type: "status", engine: "up", last_seq: server.seq });
        } else if (msg.type === "command") answer(msg, this);
      }, 0);
    }
    close() {
      this.closed = true;
      this.onclose?.({});
    }
  };
  return server;
}

// ---- the page, loaded as the browser loads the built file ------------------------------------------

function loadPage() {
  const html = source("./index.html");
  const scripts = [...html.matchAll(/<script>([\s\S]*?)<\/script>/g)].map((m) => m[1]);
  const pageScript = scripts.find((s) => s.includes("const DEFAULT_GATEWAY"));
  assert.ok(pageScript, "the page script");
  // The template carries one marker per library, in this order (see build.sh).
  assert.deepEqual(scripts.filter((s) => /^\/\*__[A-Z]+_JS__\*\/$/.test(s.trim())).map((s) => s.trim()), [
    "/*__RECORDER_JS__*/", "/*__GATEWAY_JS__*/", "/*__SCENARIOS_JS__*/", "/*__MODEL_JS__*/",
  ]);
  const server = fakeGateway();
  const document = stubDocument();
  const intervals = [];
  const ctx = vm.createContext({
    document,
    location: { search: "", hash: "" },
    WebSocket: server.Socket,
    URLSearchParams,
    performance,
    console,
    setTimeout,
    clearTimeout,
    setInterval: (fn, ms) => {
      const t = setInterval(fn, ms);
      intervals.push(t);
      return t;
    },
    clearInterval,
    addEventListener() {},
  });
  for (const f of ["recorder.js", "gateway.js", "scenarios.js", "model.js"]) vm.runInContext(source(`./${f}`), ctx, { filename: f });
  vm.runInContext(pageScript, ctx, { filename: "index.html" });
  const run = (code) => vm.runInContext(code, ctx);
  // A plain copy of a value of the page's realm (its arrays have another Array prototype).
  const val = (code) => JSON.parse(run(`JSON.stringify(${code})`));
  const stop = () => intervals.forEach(clearInterval);
  return { ctx, run, val, document, server, stop };
}

/** The rendered output of the regions that show data (the static docs, controls and drills excluded). */
const DATA_REGIONS = [
  "banners", "scenario", "timeline", "tl-run", "tl-label", "user-lane", "contract", "mm-lane", "ledger-table",
  "ledger-note", "run-progress", "runs-table", "runlog", "trace-head", "log", "inspector", "tb-status",
];
const rendered = (document) => DATA_REGIONS.map((id) => `<${id}>${document.getElementById(id).innerHTML}</${id}>`).join("\n");

await test("an account view renders only what the gateway sent it: no driver detail, steps, counts, verdicts or timing, and no other participant", async () => {
  const { run, val, document, server, stop } = loadPage();
  try {
    document.getElementById("scenario-pick").value = "happy_path";
    for (let i = 0; i < 100 && run("page.conn") !== "connected"; i++) await sleep(10);
    assert.equal(run("page.conn"), "connected");
    run('setView("mm1")');
    await run('startRuns("one", "happy_path")');
    const result = val("page.runs[0]");
    // The run FAILed on Alice's request, with the driver's sentence naming her step.
    assert.equal(result.pass, false);
    assert.match(result.detail, /req \(alice SubmitRequest\): engine unavailable/);
    assert.ok(result.log.some((l) => l.includes("fund_alice")), result.log.join("\n"));
    const { alice, mm1, mm2, carol } = result.accounts;
    // The view connection said hello as this run's MM1 and heard its own deposit and the public listing.
    const viewSocket = server.sockets.find((s) => s.view?.account === mm1);
    assert.ok(viewSocket, "a second connection in MM1's view");
    await sleep(150);
    run("render()");
    assert.deepEqual(val("V.M.accounts.map((a) => a.id)"), [mm1]);
    const out = rendered(document);
    // Exact strings, case-sensitive: the page's static copy may say "command" or "oracle"; these are data.
    for (const needle of [
      "engine unavailable", // the run's detail
      "SubmitRequest", "fund_alice", "fund_mm2", "req (", // the driver's step names and log
      "step ", "commands answered", " commands ·", // step N of M, command counts
      "FAIL", "PASS", "Running", "STOPPED", // verdicts
      "latency", "p50", "p95", "Wall", // timing
      "Alice", "MM2", "Carol", "Oracle (run", // other participants by name: the page knows them, MM1 doesn't
      String(alice), String(mm2), // and by account id (Carol's comes in a public event: allowed, as an id)
      `${result.id} ·`, // the driver's run list
    ]) {
      const at = out.indexOf(needle);
      assert.ok(at < 0, `"${needle}" in MM1's view:\n${out.slice(Math.max(0, at - 200), at + 200)}`);
    }
    // In their place, the sentence.
    for (const id of ["scenario", "runs-table"]) {
      assert.ok(document.getElementById(id).innerHTML.includes("Run results and timing are shown in the Admin view."), id);
    }
    // What MM1 did receive is there: its own balance (named, it is the viewer) and the public listing.
    assert.match(out, /MM1 \(run …\d{4}\)/);
    assert.ok(out.includes("$1,000.00"), "MM1's own balance");
    assert.ok(out.includes(`Contract #${result.contract}`), "the public listing");
    assert.ok(out.includes(`by #${carol}`), "a public event's account, as the gateway sent it: an id");

    // Not vacuous: the same page in the Admin view shows the driver's detail, steps and verdict.
    run('setView("admin")');
    run("render()");
    const admin = rendered(document);
    assert.ok(admin.includes("engine unavailable"), "the detail in the Admin view");
    assert.ok(admin.includes("fund_alice"), "the run log in the Admin view");
    assert.ok(admin.includes("FAIL"), "the verdict in the Admin view");
    assert.ok(admin.includes(String(alice)), "Alice's account in the Admin view");
  } finally {
    stop();
    run("driver.dispose(); closeViewConn(); gw.close();");
  }
});

// ---- multi-leg runs: the simulated engine plays the two multi-leg scenarios ---------------------------

/** A page on the simulated multi-leg engine, connected. */
async function simPage() {
  const page = loadPage();
  page.server.sim = true;
  for (let i = 0; i < 100 && page.run("page.conn") !== "connected"; i++) await sleep(10);
  assert.equal(page.run("page.conn"), "connected");
  return page;
}
/** The text of a rendered region. */
const text = (document, id) => document.getElementById(id).textContent;
/** How many times `re` matches in `s`. */
const count = (s, re) => (s.match(new RegExp(re, "g")) ?? []).length;
/** Whether the price `p` ("0.62") is shown as a price, not as the tail or head of a longer number. */
const showsPrice = (out, p) => new RegExp(`(?<![0-9.])${p.replace(".", "\\.")}(?![0-9])`).test(out);

await test("multi_leg_leg2_fails, admin: every leg and quote is listed, the request is Expired with no position and every hold back", async () => {
  const { run, val, document, stop } = await simPage();
  try {
    document.getElementById("scenario-pick").value = "multi_leg_leg2_fails";
    await run('startRuns("one", "multi_leg_leg2_fails")');
    const result = val("page.runs[0]");
    assert.equal(result.pass, true, result.detail);
    // The run's log names its markets, not a contract that was never listed.
    assert.match(result.log[0], new RegExp(`markets ${result.t0 * 10 + 1}\\.\\.${result.t0 * 10 + 3}$`));
    await sleep(150);
    run("render()");
    const { req, q1, q3 } = result.labels;
    // While it was open (the first batch with its legs): three open legs, "no quote yet" on each, and in the
    // market makers' cards the same legs with every limit sealed, even to the admin.
    const opened = val("V.r.batches.findIndex((b) => b.events.some((e) => RfqModel.variant(e.event) === 'RequestOpened'))");
    run(`ui.pos = ${opened}; render()`);
    const openLane = text(document, "user-lane");
    assert.ok(openLane.includes("Open") && count(openLane, "no quote yet") === 3 && count(openLane, "Limit 0\\.70") === 3, openLane);
    const openMm = text(document, "mm-lane");
    for (const leg of [1, 2, 3]) assert.equal(count(openMm, `Leg ${leg} of 3`), 2, `leg ${leg} in each MM's card`);
    assert.equal(count(openMm, "limit sealed"), 6);
    assert.ok(!showsPrice(openMm, "0.70"), "the limit in an MM's card");
    run("ui.pos = null; render()");
    const lane = text(document, "user-lane");
    // The request card: every leg with its market, side, qty and (admin, and the requester's own card) the limit.
    assert.ok(lane.includes(`Request #${req}`) && lane.includes("Expired"), lane);
    for (const leg of [1, 2, 3]) assert.match(lane, new RegExp(`Leg ${leg} of 3`), `leg ${leg}`);
    for (const city of ["Paris", "London", "Rome"]) assert.ok(lane.includes(`Will it rain in ${city} on 2026-11-01?`), city);
    assert.equal(count(lane, "Buy YES × 400"), 3);
    assert.equal(count(lane, "Limit 0\\.70"), 3);
    // Nothing filled and every hold released, said once on the card; leg 2 never had a quote.
    assert.match(lane, /No leg was filled/);
    assert.match(lane, /every hold (was )?released/i);
    assert.match(lane, /Leg 2 of 3[^]*no quote/i, "the leg nobody quoted");
    assert.deepEqual(val("V.M.positions"), []);
    // Alice, MM1, MM2 and Carol: every balance is back to the $1,000 the operator deposited, nothing held.
    assert.deepEqual(val("V.M.accounts.map((a) => [a.balance.free, a.balance.reserved, a.balance.escrowed])"), Array(4).fill([1e9, 0, 0]));
    // MM lane: each quote with its own leg, Lost, hold released.
    const mm = text(document, "mm-lane");
    assert.match(mm, new RegExp(`Quote #${q1}[^]*?leg 1 of 3`, "i"));
    assert.match(mm, new RegExp(`Quote #${q3}[^]*?leg 3 of 3`, "i"));
    assert.equal(count(mm, "Lost"), 2);
    assert.equal(count(mm, "Hold released"), 2);
    // The contract area: all three markets the run listed, each tied to its leg, with a timeline that makes sense.
    const contract = text(document, "contract");
    for (const city of ["Paris", "London", "Rome"]) assert.ok(contract.includes(`Will it rain in ${city} on 2026-11-01?`), `${city} in the contract area`);
    for (const leg of [1, 2, 3]) assert.match(contract, new RegExp(`Leg ${leg} of 3 of request #${req}`), `market of leg ${leg}`);
    // Compact cards, not three full ones, and no timeline counting from a market id (a time ~10^13 s away).
    assert.equal(count(document.getElementById("contract").innerHTML, 'class="panel contract market"'), 3);
    assert.ok(!/t\+-/.test(contract) && !contract.includes("trading closes"), "a timeline measured from the wrong start");
    // The deadlines stay one click away: the inspector's facts for a market are relative to the run's t0.
    run(`selected = { kind: "contract", id: ${result.t0 * 10 + 2} }; render()`);
    const inspector = text(document, "inspector");
    assert.match(inspector, /Trading closes t\+5\.000 s/);
    assert.match(inspector, /Hard deadline t\+15\.000 s/);
    run("selected = null; render()");
    assert.deepEqual(val("V.M.contracts.map((c) => c.id)"), [1, 2, 3].map((k) => result.t0 * 10 + k));
    // The trace says it too, and the balances under it are the release (display diffs of the engine's balances).
    const trace = text(document, "log");
    assert.match(trace, /none of its 3 legs filled/);
    assert.match(trace, /reserved \$0\.00 \(−\$840\.00\)/);
    assert.match(trace, /reserved \$0\.00 \(−\$152\.00\)/);
    assert.match(trace, /reserved \$0\.00 \(−\$160\.00\)/);
  } finally {
    stop();
    run("driver.dispose(); closeViewConn(); gw.close();");
  }
});

await test("multi_leg_happy, admin: Presented lists a pick per leg, Filled a position per leg, and each market holds its own", async () => {
  const { run, val, document, stop } = await simPage();
  try {
    document.getElementById("scenario-pick").value = "multi_leg_happy";
    await run('startRuns("one", "multi_leg_happy")');
    const result = val("page.runs[0]");
    assert.equal(result.pass, true, result.detail);
    const { req, q1, q2, q3 } = result.labels;
    // Scrub to the batch that presented it.
    const presented = val("V.r.batches.findIndex((b) => b.events.some((e) => RfqModel.variant(e.event) === 'RequestPresented'))");
    assert.ok(presented > 0);
    run(`ui.pos = ${presented}; render()`);
    let lane = text(document, "user-lane");
    assert.ok(lane.includes("Presented"), lane);
    for (const [leg, q, p] of [[1, q1, "0.62"], [2, q2, "0.60"], [3, q3, "0.64"]]) {
      assert.match(lane, new RegExp(`Leg ${leg} of 3[^]*?Best quote #${q} at ${p.replace(".", "\\.")}`), `pick of leg ${leg}`);
    }
    assert.equal(count(lane, "Best quote"), 3);
    assert.match(lane, /Accept by/);
    assert.equal(count(text(document, "mm-lane"), "Selected"), 3);
    // The end: Filled, one position per leg, each on its own market.
    run("ui.pos = null; render()");
    lane = text(document, "user-lane");
    assert.ok(lane.includes("Filled"), lane);
    for (const [leg, pos] of [[1, 1], [2, 2], [3, 3]]) assert.match(lane, new RegExp(`Leg ${leg} of 3[^]*?Position #${pos}`), `position of leg ${leg}`);
    assert.equal(count(lane, "Position #"), 3);
    assert.deepEqual(val("V.M.positions.map((p) => [p.id, p.leg, p.contract])"), [1, 2, 3].map((k, i) => [k, i, result.t0 * 10 + k]));
    const contract = text(document, "contract");
    assert.equal(count(contract, "400 shares"), 3, "a position in each market");
    const mm = text(document, "mm-lane");
    for (const [q, leg] of [[q1, 1], [q2, 2], [q3, 3]]) assert.match(mm, new RegExp(`Quote #${q}[^]*?leg ${leg} of 3`, "i"), `quote ${q}`);
    assert.equal(count(mm, "Filled"), 3);
  } finally {
    stop();
    run("driver.dispose(); closeViewConn(); gw.close();");
  }
});

await test("view as MM2 during a multi-leg run: its own leg and balance, the public legs; no MM1 prices, no Alice limit, no markets' terms", async () => {
  const { run, val, document, server, stop } = await simPage();
  try {
    document.getElementById("scenario-pick").value = "multi_leg_happy";
    run('setView("mm2")');
    await run('startRuns("one", "multi_leg_happy")');
    const result = val("page.runs[0]");
    assert.equal(result.pass, true, result.detail);
    const { alice, mm1, mm2 } = result.accounts;
    const { req, q1, q2, q3 } = result.labels;
    assert.ok(server.sockets.find((s) => s.view?.account === mm2), "a second connection in MM2's view");
    await sleep(150);
    run("render()");
    assert.deepEqual(val("V.M.accounts.map((a) => a.id)"), [mm2]);
    const out = rendered(document);
    // Not vacuous: MM2 sees the request's three public legs, its own quote on leg 2 and its own money.
    assert.ok(out.includes(`#${req}`), "the public request");
    assert.equal(val("V.M.requests[0].legs.length"), 3);
    assert.match(text(document, "mm-lane"), new RegExp(`Quote #${q2}[^]*?leg 2 of 3`, "i"), "MM2's own quote and its leg");
    assert.ok(showsPrice(out, "0.60"), "MM2's own price");
    assert.ok(out.includes("$840.00") && out.includes("$160.00"), "MM2's own balance");
    // Sealed: MM1's legs (0.62 and 0.64, its quotes and money), Alice (her limit, her request's owner, her money), and the markets' questions.
    for (const needle of [String(alice), String(mm1), `Quote #${q1}`, `Quote #${q3}`, "Alice", "MM1", "Will it rain", "FAIL", "PASS", "multi_leg_happy ·", "latency"]) {
      const at = out.indexOf(needle);
      assert.ok(at < 0, `"${needle}" in MM2's view:\n${out.slice(Math.max(0, at - 200), at + 200)}`);
    }
    for (const sealedPrice of ["0.62", "0.64", "0.70"]) assert.ok(!showsPrice(out, sealedPrice), `${sealedPrice} in MM2's view`);
    for (const money of ["$152.00", "$296.00", "$744.00", "$256.00", "$704.00", "$248.00"]) assert.ok(!out.includes(money), `${money} in MM2's view`);
    // While the request was open, MM2 saw its three legs with each limit marked sealed.
    const opened = val("V.r.batches.findIndex((b) => b.events.some((e) => RfqModel.variant(e.event) === 'RequestOpened'))");
    assert.ok(opened >= 0);
    run(`ui.pos = ${opened}; render()`);
    const early = text(document, "mm-lane");
    for (const leg of [1, 2, 3]) assert.match(early, new RegExp(`Leg ${leg} of 3`), `leg ${leg}`);
    assert.equal(count(early, "Buy YES × 400"), 3);
    assert.equal(count(early, "limit sealed"), 3, "the limit is marked sealed, not shown");
    const openOut = rendered(document);
    for (const sealedPrice of ["0.62", "0.64", "0.70"]) assert.ok(!showsPrice(openOut, sealedPrice), `${sealedPrice} in MM2's view of the open request`);
    assert.ok(!openOut.includes(String(alice)) && !openOut.includes("Will it rain"));
    run("ui.pos = null; render()");
    assert.ok(document.getElementById("runs-table").innerHTML.includes("Run results and timing are shown in the Admin view."));
    // The page's own view of the same run, for contrast: the admin sees every leg's price and the limit.
    run('setView("admin")');
    run("render()");
    const admin = rendered(document);
    for (const shown of ["0.62", "0.64", "0.70", "Will it rain in Rome"]) assert.ok(admin.includes(shown), `${shown} in the Admin view`);
  } finally {
    stop();
    run("driver.dispose(); closeViewConn(); gw.close();");
  }
});

await test("the multi-leg tab says Built, has a Run button per scenario (only for one in the catalogue), and maps the walkthrough to them", async () => {
  const { run, document, stop } = loadPage();
  try {
    run('ui.tab = "ml"; render()');
    const tab = document.getElementById("panels").innerHTML;
    assert.match(tab, /Multi-leg <small>— Built<\/small>/, "the tab's label");
    assert.match(tab, /<span class="built">Built<\/span>/);
    for (const stale of ["Designed, not built (P6)", "Unsupported", "rejects more than one leg"]) assert.ok(!tab.includes(stale), `"${stale}" is stale`);
    for (const name of ["multi_leg_leg2_fails", "multi_leg_happy"]) {
      assert.match(tab, new RegExp(`data-action="run-scenario" data-scenario="${name}"`), `a Run button for ${name}`);
    }
    // The three steps stay; steps 1 and 2 are leg2_fails, and step 3 is pinned by an engine test.
    for (const step of ["1. Before the deadline", "2. The deadline fires", "3. Alternative path"]) assert.ok(tab.includes(step), step);
    assert.ok(tab.includes("a_selected_leg_cannot_be_withdrawn"));
    assert.equal(tab.split("Scenario multi_leg_leg2_fails").length - 1, 2, "steps 1 and 2 name the scenario that plays them");
    // Gated on the catalogue: a scenario that is not in it has no button.
    run('globalThis.dropped = RfqScenarios.scenarios.splice(RfqScenarios.scenarios.findIndex((s) => s.name === "multi_leg_happy"), 1); render()');
    const gated = document.getElementById("panels").innerHTML;
    assert.ok(!gated.includes('data-scenario="multi_leg_happy"'), "a button for a scenario the catalogue lacks");
    assert.ok(gated.includes('data-scenario="multi_leg_leg2_fails"'));
    run("RfqScenarios.scenarios.push(...globalThis.dropped); render()");
    assert.ok(document.getElementById("panels").innerHTML.includes('data-scenario="multi_leg_happy"'));
  } finally {
    stop();
    run("driver.dispose(); closeViewConn(); gw.close();");
  }
});

console.log(failed ? `${failed} FAILED` : "all page tests passed");
process.exit(failed ? 1 : 0);
