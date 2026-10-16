#!/usr/bin/env node
// End-to-end smoke test of the HTTP API (docs/specs/read-api.md), against the Docker stack:
//
//   docker compose up -d aeron engine gateway
//   node scripts/http-smoke.mjs [http://127.0.0.1:18081] [ws://127.0.0.1:18080]
//
// The HTTP base is the first argument, else RFQ_HTTP_URL; the WebSocket URL the second, else
// RFQ_GATEWAY_URL (as gateway-smoke.mjs takes its own).
//
// One whole trade over HTTP, with only a WebSocket listening (as admin, recording every batch):
// the operator funds three fresh accounts and lists a contract (`POST /v1/commands`), Alice asks
// for 1000 YES at a limit of 0.70 (`POST /v1/requests`), MM1 quotes 0.62 and MM2 0.60
// (`POST /v1/quotes`), and after the response deadline Alice accepts what `GET
// /v1/accounts/{alice}/requests` says was presented (`POST /v1/commands`). It then reads back,
// over HTTP, and checks that:
//   - every write answers 200 with its batch: seq, nonce, latency, events, balances;
//   - Alice's position is YES with a $600 stake, and $600 of her balance is escrowed;
//   - MM1's quote is Lost, MM2's Filled;
//   - the admin trace holds RequestOpened, QuoteSubmitted x2, RequestPresented, QuoteFilled and
//     RequestFilled in order, with the commands behind them, and every batch the WebSocket
//     recorded from the first deposit on is in the trace, equal;
//   - MM1's trace never shows MM2's quote or Alice's balances;
//   - a command the engine refuses is 422 with its reason (and consumes its nonce), a bad body is
//     400, a foreign Origin is 403, a page's Origin gets its CORS headers.
// The accounts are fresh each run (t0 * 100 + id, like rfq-demo), and so is the contract (id t0),
// so it runs against a long-lived engine. Prints `HTTP SMOKE OK` and exits 0, or the first
// failure and exits 1. Needs Node 22 or later (the global fetch and WebSocket).

import { isDeepStrictEqual } from 'node:util';

const HTTP = (process.argv[2] ?? process.env.RFQ_HTTP_URL ?? 'http://127.0.0.1:18081').replace(/\/+$/, '');
const WS_URL = process.argv[3] ?? process.env.RFQ_GATEWAY_URL ?? 'ws://127.0.0.1:18080';
/** How long one expected WebSocket frame may take. */
const TIMEOUT_MS = 10_000;
/** How long the engine may take to be up (it may still be starting or replaying). */
const ENGINE_UP_MS = 60_000;
/** How long the engine may take to present the request after its deadline. */
const PRESENTED_MS = 15_000;
/** Engine-time lead between computing t0 and the request. */
const START_MARGIN_MS = 1_500;
const ONE_DOLLAR = 1_000_000;

const OPERATOR = 1;
const ORACLE = 4;

class Failure extends Error {}

function check(ok, message) {
  if (!ok) throw new Failure(message);
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const name = (emitted) => Object.keys(emitted.event)[0];
const body = (emitted) => Object.values(emitted.event)[0];
const fmt = (n) => (n / ONE_DOLLAR).toFixed(2);
const balanceOf = (reply, account) => reply.balances.find((b) => b.account === account)?.balance;

// ---- the WebSocket: admin's view, every frame kept ----

/** One WebSocket connection, with every frame it received, in order. */
class Conn {
  constructor(name, ws) {
    this.name = name;
    this.ws = ws;
    this.frames = [];
    this.waiters = new Set();
    this.closed = null;
    ws.onmessage = (ev) => {
      this.frames.push(JSON.parse(ev.data));
      this.wake();
    };
    ws.onclose = (ev) => {
      this.closed = `connection closed (${ev.code} ${ev.reason})`;
      this.wake();
    };
  }

  static open(name, url) {
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(url);
      const conn = new Conn(name, ws);
      ws.onopen = () => resolve(conn);
      ws.onerror = () => reject(new Failure(`${name}: cannot connect to ${url} (is the gateway up?)`));
    });
  }

  send(msg) {
    this.ws.send(JSON.stringify(msg));
  }

  wake() {
    for (const waiter of [...this.waiters]) waiter();
  }

  /** The first frame at index `from` or later that satisfies `pred`, waiting up to `ms`. */
  waitFor(what, pred, { ms = TIMEOUT_MS, from = 0 } = {}) {
    return new Promise((resolve, reject) => {
      let timer;
      const done = () => {
        clearTimeout(timer);
        this.waiters.delete(look);
      };
      const look = () => {
        for (let i = from; i < this.frames.length; i++) {
          if (pred(this.frames[i])) {
            done();
            resolve({ frame: this.frames[i], index: i });
            return;
          }
        }
        if (this.closed) {
          done();
          reject(new Failure(`${this.name}: ${this.closed} while waiting for ${what}`));
        }
      };
      timer = setTimeout(() => {
        done();
        reject(new Failure(`${this.name}: no ${what} within ${ms} ms`));
      }, ms);
      this.waiters.add(look);
      look();
    });
  }

  close() {
    this.ws.close();
  }
}

/** Hello, then welcome; waits for `status up` if the engine is not up yet. */
async function hello(conn, view) {
  const from = conn.frames.length;
  conn.send({ type: 'hello', view });
  const { frame: welcome } = await conn.waitFor('welcome', (f) => f.type === 'welcome', { from });
  if (welcome.engine !== 'up') {
    console.log(`${conn.name}: engine down, waiting up to ${ENGINE_UP_MS / 1000} s for it`);
    await conn.waitFor('status up', (f) => f.type === 'status' && f.engine === 'up', { ms: ENGINE_UP_MS, from });
  }
  return welcome;
}

/** The batches the connection recorded, in arrival order; chunks of one `seq` joined, as the dashboard does. */
function recordedBatches(conn) {
  const out = [];
  for (const f of conn.frames) {
    if (f.type !== 'batch') continue;
    const { type, ...batch } = f;
    const last = out[out.length - 1];
    if (last && last.seq === batch.seq) {
      last.events.push(...batch.events);
      last.balances = batch.balances;
    } else {
      out.push({ ...batch, events: [...batch.events] });
    }
  }
  return out;
}

// ---- HTTP ----

/** One request. `json` is the parsed body (null if it is not JSON). */
async function call(method, path, { payload, headers = {} } = {}) {
  const init = { method, headers: { ...headers } };
  if (payload !== undefined) {
    init.headers['Content-Type'] = 'application/json';
    init.body = typeof payload === 'string' ? payload : JSON.stringify(payload);
  }
  let res;
  try {
    res = await fetch(HTTP + path, init);
  } catch (e) {
    throw new Failure(`${method} ${path}: cannot reach ${HTTP} (${e.cause?.code ?? e.message}); is the gateway up?`);
  }
  const text = await res.text();
  let json = null;
  try {
    json = JSON.parse(text);
  } catch {
    // not JSON
  }
  return { status: res.status, json, text, headers: res.headers };
}

/** A `GET` that must answer 200, waiting for the replica to reach `minSeq` if given. */
async function read(path, minSeq) {
  const url = minSeq === undefined ? path : `${path}${path.includes('?') ? '&' : '?'}min_seq=${minSeq}`;
  const r = await call('GET', url);
  check(r.status === 200, `GET ${url}: ${r.status} ${r.text}`);
  check(Number.isInteger(r.json.as_of_seq), `GET ${url}: no as_of_seq in ${r.text}`);
  return r.json;
}

/** Every write answered, for the table at the end. */
const results = [];

/** A `POST` that must answer 200 with the command's batch. */
async function write(id, path, payload) {
  const r = await call('POST', path, { payload });
  check(r.status === 200, `${id}: POST ${path} answered ${r.status} ${r.text}`);
  const reply = r.json;
  check(reply.rejected === null, `${id}: rejected by the engine: ${reply.rejected}`);
  check(Number.isInteger(reply.seq) && reply.seq > 0, `${id}: bad seq in ${r.text}`);
  check(Number.isInteger(reply.nonce), `${id}: bad nonce in ${r.text}`);
  check(Number.isInteger(reply.latency_us) && reply.latency_us >= 0, `${id}: bad latency in ${r.text}`);
  check(Array.isArray(reply.events) && Array.isArray(reply.balances), `${id}: no events and balances in ${r.text}`);
  results.push({ id, route: path, nonce: reply.nonce, seq: reply.seq, latency_us: reply.latency_us });
  return reply;
}

/** The trace of `view` after `afterSeq`, every page, once the replica has reached `minSeq`. */
async function trace(view, afterSeq, minSeq) {
  const entries = [];
  let after = afterSeq;
  for (;;) {
    const page = await read(`/v1/trace?view=${view}&after_seq=${after}&limit=1000`, minSeq);
    entries.push(...page.entries);
    if (page.next_after_seq === null) return entries;
    check(page.next_after_seq > after, `trace paging does not advance: ${page.next_after_seq} after ${after}`);
    after = page.next_after_seq;
  }
}

async function main() {
  console.log(`http smoke against ${HTTP} (websocket ${WS_URL})`);
  const admin = await Conn.open('admin', WS_URL);
  const welcome = await hello(admin, 'admin');
  // The gateway's wall clock is the engine's clock; align with it (to within the round trip).
  const offset = welcome.now - Date.now();
  const engineNow = () => Date.now() + offset;

  const status = await call('GET', '/v1/status');
  check(status.status === 200, `GET /v1/status: ${status.status} ${status.text}`);
  check(status.json.engine === 'up', `the HTTP API says the engine is ${status.json.engine}`);
  console.log(`status: engine up, replica ${status.json.replica.state} at seq ${status.json.replica.last_seq}`);

  const t0 = engineNow() + START_MARGIN_MS;
  const ALICE = t0 * 100 + 10;
  const MM1 = t0 * 100 + 20;
  const MM2 = t0 * 100 + 21;
  check(Number.isSafeInteger(MM2), 'account ids overflow a JS number');
  console.log(`run t0=${t0}: contract ${t0}, alice ${ALICE}, mm1 ${MM1}, mm2 ${MM2}`);

  // ---- the trade, over HTTP only ----
  let firstSeq = null;
  for (const [id, to] of [['fund_alice', ALICE], ['fund_mm1', MM1], ['fund_mm2', MM2]]) {
    const r = await write(id, '/v1/commands', { actor: OPERATOR, action: { Deposit: { to, amount: 1_000 * ONE_DOLLAR } } });
    // The reply is the operator's view of its command: the deposit itself is addressed to the
    // account it funds, so the new balance is read from the account, once the replica has the seq.
    const { balance } = await read(`/v1/accounts/${to}`, r.seq);
    check(balance.free === 1_000 * ONE_DOLLAR, `${id}: balance ${JSON.stringify(balance)}, expected $1000 free`);
    firstSeq ??= r.seq;
  }
  await write('list', '/v1/commands', {
    actor: OPERATOR,
    action: {
      ListContract: {
        id: t0,
        description: 'Will it rain in Paris on 2026-11-01?',
        oracle: ORACLE,
        trading_close: t0 + 5_000,
        resolution_deadline: t0 + 15_000,
        dispute_window: 2_000,
      },
    },
  });

  check(engineNow() < t0 + 100, `setup took too long: ${engineNow() - t0} ms past t0`);
  await sleep(t0 + 100 - engineNow());
  const req = await write('req', '/v1/requests', {
    actor: ALICE,
    legs: [{ contract: t0, side: 'BuyYes', qty: 1_000, limit_price: 7_000 }],
    response_deadline: t0 + 1_500,
  });
  const request = req.seq; // a request's id is the seq of the command that opened it
  check(req.events.some((e) => name(e) === 'RequestOpened' && body(e).request === request), `req: no RequestOpened in ${JSON.stringify(req.events)}`);
  check(balanceOf(req, ALICE)?.reserved > 0, `req: Alice's hold not in the batch: ${JSON.stringify(req.balances)}`);
  const quote = (actor, price) => ({ actor, request, leg: 0, price, size: 1_000, expires_at: t0 + 10_000 });
  const q1 = await write('q1', '/v1/quotes', quote(MM1, 6_200));
  check(q1.events.some((e) => name(e) === 'QuoteSubmitted' && body(e).quote === q1.seq), `q1: no QuoteSubmitted for its own quote`);
  check(balanceOf(q1, MM1)?.reserved > 0, `q1: MM1's hold not in the batch`);
  const q2 = await write('q2', '/v1/quotes', quote(MM2, 6_000));
  check(balanceOf(q2, MM2)?.reserved > 0, `q2: MM2's hold not in the batch`);

  // The engine presents the best quote at the response deadline; Alice reads what she was told.
  const until = Date.now() + PRESENTED_MS;
  let own;
  for (;;) {
    const { requests } = await read(`/v1/accounts/${ALICE}/requests`, q2.seq);
    own = requests.find((r) => r.id === request);
    check(own, `request ${request} is not among Alice's: ${JSON.stringify(requests)}`);
    if (own.legs.every((leg) => leg.selected)) break;
    check(Date.now() < until, `request ${request} not presented within ${PRESENTED_MS / 1000} s: ${JSON.stringify(own)}`);
    await sleep(200);
  }
  check(own.legs.length === 1 && own.legs[0].limit_price === 7_000, `Alice's request reads back as ${JSON.stringify(own)}`);
  const picks = own.legs.map((leg, i) => ({ leg: i, quote: leg.selected.quote, price: leg.selected.price }));
  check(picks.length === 1 && picks[0].quote === q2.seq && picks[0].price === 6_000, `presented ${JSON.stringify(picks)}, expected MM2's quote ${q2.seq} at 6000`);
  const accept = await write('accept', '/v1/commands', { actor: ALICE, action: { Accept: { request, picks } } });
  const alice = balanceOf(accept, ALICE);
  check(alice?.escrowed === 600 * ONE_DOLLAR, `accept: Alice escrowed ${JSON.stringify(alice)}, expected $600`);

  // ---- the same, read back over HTTP ----
  const positions = (await read(`/v1/accounts/${ALICE}/positions`, accept.seq)).positions;
  check(positions.length === 1, `Alice has ${positions.length} positions, expected 1: ${JSON.stringify(positions)}`);
  const [position] = positions;
  check(position.side === 'Yes' && position.stake === 600 * ONE_DOLLAR, `Alice's position: ${JSON.stringify(position)}, expected YES with a $600 stake`);
  check(position.contract === t0 && position.request === request && position.qty === 1_000 && position.settled === false, `Alice's position: ${JSON.stringify(position)}`);

  const account = await read(`/v1/accounts/${ALICE}`, accept.seq);
  check(account.balance.escrowed === 600 * ONE_DOLLAR, `Alice's account: ${JSON.stringify(account.balance)}, expected $600 escrowed`);
  check(account.last_nonce === accept.nonce, `Alice's last nonce is ${account.last_nonce}, expected ${accept.nonce}`);

  const stateOf = async (account, id) => (await read(`/v1/accounts/${account}/quotes`, accept.seq)).quotes.find((q) => q.id === id)?.state;
  const [state1, state2] = [await stateOf(MM1, q1.seq), await stateOf(MM2, q2.seq)];
  check(state1 === 'Lost', `MM1's quote is ${state1}, expected Lost`);
  check(state2 === 'Filled', `MM2's quote is ${state2}, expected Filled`);

  // A command the engine refuses is 422 with its reason, and consumes its nonce.
  const refused = await call('POST', '/v1/commands', { payload: { actor: ALICE, action: { Withdraw: { amount: 1_000_000 * ONE_DOLLAR } } } });
  check(refused.status === 422, `an overdraft answered ${refused.status} ${refused.text}, expected 422`);
  check(refused.json.rejected === 'InsufficientFunds', `an overdraft was rejected as ${refused.json.rejected}`);
  check(refused.json.nonce === accept.nonce + 1, `the refused command took nonce ${refused.json.nonce}, expected ${accept.nonce + 1}`);
  check((await read(`/v1/accounts/${ALICE}`, refused.json.seq)).last_nonce === refused.json.nonce, 'a refused command did not consume its nonce');

  // Writers racing for one account (MM2's): the hub queues them, and each reply holds its own
  // command's batch, never a neighbour's: the withdrawal of its own amount.
  const amounts = [1, 2, 3, 4, 5];
  const burst = await Promise.all(
    amounts.map((amount) => write(`burst_${amount}`, '/v1/commands', { actor: MM2, action: { Withdraw: { amount } } })),
  );
  check(new Set(burst.map((r) => r.seq)).size === amounts.length, `the burst shares seqs: ${burst.map((r) => r.seq)}`);
  check(new Set(burst.map((r) => r.nonce)).size === amounts.length, `the burst shares nonces: ${burst.map((r) => r.nonce)}`);
  burst.forEach((r, i) => {
    const withdrawals = r.events.filter((e) => name(e) === 'Withdrawn');
    check(withdrawals.length === 1 && body(withdrawals[0]).amount === amounts[i], `burst_${amounts[i]} got ${JSON.stringify(r.events)}`);
  });

  // ---- the trace ----
  // Let anything still on its way reach the WebSocket, then compare with what it recorded.
  await sleep(300);
  const recorded = recordedBatches(admin).filter((b) => b.seq >= firstSeq);
  const lastSeq = recorded[recorded.length - 1].seq;
  const entries = await trace('admin', firstSeq - 1, lastSeq);
  check(entries.length >= recorded.length, `the admin trace has ${entries.length} entries, the WebSocket recorded ${recorded.length}`);
  check(entries.every((e, i) => i === 0 || e.seq > entries[i - 1].seq), 'the trace is not in seq order');
  const bySeq = new Map(entries.map((e) => [e.seq, e]));
  for (const batch of recorded) {
    const entry = bySeq.get(batch.seq);
    check(entry, `seq ${batch.seq} was on the WebSocket but is not in the trace`);
    const { command, ...seen } = entry;
    check(isDeepStrictEqual(seen, batch), `seq ${batch.seq} differs.\n  websocket: ${JSON.stringify(batch)}\n  trace:     ${JSON.stringify(seen)}`);
  }

  const ours = (e) => {
    const b = body(e);
    switch (name(e)) {
      case 'RequestOpened':
      case 'QuoteSubmitted':
      case 'RequestPresented':
      case 'RequestFilled':
        return b.request === request;
      case 'QuoteFilled':
        return b.quote === q1.seq || b.quote === q2.seq;
      default:
        return false;
    }
  };
  const events = entries.flatMap((entry) => entry.events.filter(ours).map((e) => ({ seq: entry.seq, e })));
  const expect = ['RequestOpened', 'QuoteSubmitted', 'QuoteSubmitted', 'RequestPresented', 'QuoteFilled', 'RequestFilled'];
  check(isDeepStrictEqual(events.map(({ e }) => name(e)), expect), `trace events: ${events.map(({ e }) => name(e)).join(', ')}; expected ${expect.join(', ')}`);

  // The command behind each entry: what no event carries.
  const commandAt = (seq) => bySeq.get(seq)?.command;
  const opened = commandAt(request);
  check(opened?.actor === ALICE && opened.nonce === req.nonce && opened.action.SubmitRequest?.legs[0].limit_price === 7_000, `trace command at seq ${request}: ${JSON.stringify(opened)}`);
  const quoted = commandAt(q1.seq);
  check(quoted?.actor === MM1 && quoted.action.SubmitQuote?.size === 1_000, `trace command at seq ${q1.seq}: ${JSON.stringify(quoted)}`);
  const accepted = commandAt(accept.seq);
  check(accepted?.actor === ALICE && accepted.nonce === accept.nonce && isDeepStrictEqual(accepted.action.Accept?.picks, picks), `trace command at seq ${accept.seq}: ${JSON.stringify(accepted)}`);
  check(commandAt(refused.json.seq)?.action.Withdraw !== undefined, `the refused command is not in the trace at seq ${refused.json.seq}`);
  burst.forEach((r, i) => {
    const command = commandAt(r.seq);
    check(command?.actor === MM2 && command.nonce === r.nonce && command.action.Withdraw?.amount === amounts[i], `trace command at seq ${r.seq} (burst_${amounts[i]}): ${JSON.stringify(command)}`);
  });

  // MM1's trace: its own commands and what is public or addressed to it, nothing of MM2 or Alice.
  const mm1Entries = await trace(MM1, firstSeq - 1, lastSeq);
  let own1 = 0;
  for (const entry of mm1Entries) {
    for (const e of entry.events) {
      const a = e.audience;
      check(a === 'Public' || a?.Account === MM1, `MM1's trace shows an event addressed to ${JSON.stringify(a)}: ${JSON.stringify(e.event)} (seq ${entry.seq})`);
      if (a?.Account === MM1) own1++;
      check(!(name(e) === 'QuoteSubmitted' && body(e).quote === q2.seq), `MM1's trace shows MM2's quote (seq ${entry.seq})`);
    }
    for (const b of entry.balances) check(b.account === MM1, `MM1's trace shows the balance of account ${b.account} (seq ${entry.seq})`);
    check(entry.origin === null || entry.origin.actor === MM1, `MM1's trace shows another account's origin (seq ${entry.seq})`);
    check(entry.command === null || entry.command.actor === MM1, `MM1's trace shows another account's command (seq ${entry.seq})`);
  }
  const mm1Names = mm1Entries.flatMap((entry) => entry.events.map(name));
  check(mm1Names.includes('QuoteSubmitted') && mm1Names.includes('QuoteLost'), `MM1's trace misses its own events: ${mm1Names.join(', ')}`);
  check(!mm1Entries.some((entry) => entry.balances.some((b) => b.account === ALICE)), "MM1's trace shows Alice's balances");

  // ---- what a client gets wrong ----
  const empty = await call('POST', '/v1/requests', { payload: {} });
  check(empty.status === 400 && /^bad request: /.test(empty.json?.error ?? ''), `POST {} answered ${empty.status} ${empty.text}, expected 400 bad request`);
  const notJson = await call('POST', '/v1/commands', { payload: 'this is not json' });
  check(notJson.status === 400 && /^bad request: /.test(notJson.json?.error ?? ''), `POST of text answered ${notJson.status} ${notJson.text}, expected 400 bad request`);
  const unknownField = await call('POST', '/v1/commands', { payload: { actor: ALICE, action: { Withdraw: { amount: 1 } }, extra: 1 } });
  check(unknownField.status === 400, `POST with an unknown field answered ${unknownField.status}, expected 400`);

  const evil = { Origin: 'http://evil.example' };
  const foreignGet = await call('GET', '/v1/status', { headers: evil });
  check(foreignGet.status === 403, `GET from a foreign Origin answered ${foreignGet.status}, expected 403`);
  const foreignPost = await call('POST', '/v1/commands', { payload: { actor: ALICE, action: { Withdraw: { amount: 1 } } }, headers: evil });
  check(foreignPost.status === 403, `POST from a foreign Origin answered ${foreignPost.status}, expected 403`);
  const local = await call('GET', '/v1/status', { headers: { Origin: 'http://localhost:3000' } });
  check(local.status === 200 && local.headers.get('access-control-allow-origin') === 'http://localhost:3000', `a local page got ${local.status} and CORS ${local.headers.get('access-control-allow-origin')}`);
  check((await call('GET', '/v1/nope')).status === 404, 'an unknown route is not 404');
  check((await call('DELETE', '/v1/commands')).status === 405, 'DELETE is not 405');
  // None of the refused POSTs above ran: Alice's nonce is still the overdraft's.
  check((await read(`/v1/accounts/${ALICE}`, refused.json.seq)).last_nonce === refused.json.nonce, "a POST that was refused (bad body, foreign Origin) still moved Alice's nonce");

  for (const r of results) {
    console.log(`  ${r.id.padEnd(10)} ${r.route.padEnd(13)} nonce ${String(r.nonce).padStart(4)}  seq ${String(r.seq).padStart(5)}  latency ${r.latency_us} us`);
  }
  console.log(`  overdraft   422 ${refused.json.rejected}, nonce ${refused.json.nonce} consumed, seq ${refused.json.seq}`);
  for (const { seq, e } of events) console.log(`  trace seq ${String(seq).padStart(5)}  ${name(e)}`);
  console.log(`  alice: position ${position.side} stake $${fmt(position.stake)}, escrowed $${fmt(account.balance.escrowed)}; mm1 quote Lost, mm2 quote Filled`);
  console.log(`  admin trace: ${entries.length} entries from seq ${firstSeq}, ${recorded.length} WebSocket batches all in it, equal`);
  console.log(`  mm1 trace: ${mm1Entries.length} entries, ${own1} events of its own, nothing of anyone else`);
  admin.close();
  console.log('HTTP SMOKE OK');
}

main().then(
  () => process.exit(0),
  (e) => {
    console.error(`HTTP SMOKE FAILED: ${e instanceof Failure ? e.message : e.stack}`);
    process.exit(1);
  },
);
