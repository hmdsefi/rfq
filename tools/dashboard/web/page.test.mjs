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
  // The operator's setup goes through; Alice's request finds the engine down: the run FAILs on it.
  function answer(msg, socket) {
    server.commands.push(msg);
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
  "ledger-note", "run-progress", "runs-table", "runlog", "perf", "trace-head", "log", "inspector", "tb-status",
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
    for (const id of ["scenario", "runs-table", "perf"]) {
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

console.log(failed ? `${failed} FAILED` : "all page tests passed");
process.exit(failed ? 1 : 0);
