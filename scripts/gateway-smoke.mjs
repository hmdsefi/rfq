#!/usr/bin/env node
// End-to-end smoke test of the gateway (docs/specs/gateway.md), against the Docker stack:
//
//   docker compose up -d aeron engine gateway
//   node scripts/gateway-smoke.mjs [ws://127.0.0.1:8080]
//
// One trade over the WebSocket, as the admin view: the operator funds three fresh accounts and
// lists a contract, Alice asks for 1000 YES (limit 0.70), MM1 quotes 0.62 and MM2 0.60, and after
// the response deadline Alice accepts what the engine presented. It checks that:
//   - every command gets `sent`, then its batch, then its `result` with a latency;
//   - the events come in order: RequestOpened, QuoteSubmitted x2, RequestPresented, QuoteFilled,
//     RequestFilled;
//   - each money command's batch carries the touched balances, and Alice's escrow after the
//     accept is $600 (in micro-USD);
//   - a second connection with MM1's view gets only public events and MM1's own, only MM1's
//     balances, and origins only for MM1's commands.
// The accounts are fresh each run (t0 * 100 + id, like rfq-demo), and so is the contract (id t0),
// so it runs against a long-lived engine. Prints `GATEWAY SMOKE OK` and exits 0, or the first
// failure and exits 1. Needs Node 22 or later (the global WebSocket).

const URL = process.argv[2] ?? process.env.RFQ_GATEWAY_URL ?? 'ws://127.0.0.1:8080';
/** How long one expected frame may take. */
const TIMEOUT_MS = 10_000;
/** How long the engine may take to be up (it may still be starting or replaying). */
const ENGINE_UP_MS = 60_000;
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

const name = (emitted) => Object.keys(emitted.event)[0];
const body = (emitted) => Object.values(emitted.event)[0];
const fmt = (n) => (n / ONE_DOLLAR).toFixed(2);

/** Hello, then welcome; waits for `status up` if the engine is not up yet. */
async function hello(conn, view) {
  const from = conn.frames.length;
  conn.send({ type: 'hello', view });
  const { frame: welcome } = await conn.waitFor('welcome', (f) => f.type === 'welcome', { from });
  check(JSON.stringify(welcome.view) === JSON.stringify(view), `${conn.name}: welcome for the wrong view: ${JSON.stringify(welcome)}`);
  if (welcome.engine !== 'up') {
    console.log(`${conn.name}: engine down, waiting up to ${ENGINE_UP_MS / 1000} s for it`);
    await conn.waitFor('status up', (f) => f.type === 'status' && f.engine === 'up', { ms: ENGINE_UP_MS, from });
  }
  return welcome;
}

/** Every command sent, with its answer. */
const results = [];

/**
 * Sends `action` as `actor` and waits for its result. Checks `sent`, then the batch whose origin is
 * the command, then the `result`; and that the engine did not reject it.
 */
async function command(conn, id, actor, action) {
  const from = conn.frames.length;
  conn.send({ type: 'command', id, actor, action });
  const { frame: answer, index: at } = await conn.waitFor(
    `the result of ${id}`,
    (f) => (f.type === 'result' || f.type === 'error') && f.id === id,
    { from },
  );
  check(answer.type === 'result', `${id}: ${answer.error}`);
  const seen = conn.frames.slice(from, at);
  const sent = seen.find((f) => f.type === 'sent' && f.id === id);
  check(sent && sent.actor === actor, `${id}: no \`sent\` before its result`);
  const batch = seen.find(
    (f) => f.type === 'batch' && f.seq === answer.seq && f.origin?.actor === actor && f.origin?.nonce === sent.nonce,
  );
  check(batch, `${id}: no batch with origin {actor ${actor}, nonce ${sent.nonce}} before its result`);
  check(Number.isInteger(answer.latency_us) && answer.latency_us >= 0, `${id}: bad latency ${answer.latency_us}`);
  check(Number.isInteger(batch.time) && batch.time > 0, `${id}: batch without engine time`);
  check(batch.kind === 'User', `${id}: batch kind ${batch.kind}`);
  check(Array.isArray(batch.balances), `${id}: batch without balances`);
  const rejected = batch.events.find((e) => name(e) === 'CommandRejected');
  check(!rejected, `${id}: rejected by the engine: ${JSON.stringify(rejected?.event)}`);
  results.push({ id, action: Object.keys(action)[0], nonce: sent.nonce, seq: answer.seq, latency_us: answer.latency_us });
  return { seq: answer.seq, batch };
}

const balanceOf = (batch, account) => batch.balances.find((b) => b.account === account)?.balance;

async function main() {
  console.log(`gateway smoke against ${URL}`);
  const admin = await Conn.open('admin', URL);
  const welcome = await hello(admin, 'admin');
  // The gateway's wall clock is the engine's clock; align with it (to within the round trip).
  const offset = welcome.now - Date.now();
  const engineNow = () => Date.now() + offset;

  const t0 = engineNow() + START_MARGIN_MS;
  const ALICE = t0 * 100 + 10;
  const MM1 = t0 * 100 + 20;
  const MM2 = t0 * 100 + 21;
  check(Number.isSafeInteger(MM2), 'account ids overflow a JS number');
  console.log(`run t0=${t0}: contract ${t0}, alice ${ALICE}, mm1 ${MM1}, mm2 ${MM2}`);

  const mm1 = await Conn.open('mm1', URL);
  await hello(mm1, { account: MM1 });

  for (const [id, to] of [['fund_alice', ALICE], ['fund_mm1', MM1], ['fund_mm2', MM2]]) {
    const { batch } = await command(admin, id, OPERATOR, { Deposit: { to, amount: 1_000 * ONE_DOLLAR } });
    const b = balanceOf(batch, to);
    check(b && b.free === 1_000 * ONE_DOLLAR, `${id}: balance ${JSON.stringify(b)}, expected $1000 free`);
  }
  await command(admin, 'list', OPERATOR, {
    ListContract: {
      id: t0,
      description: 'Will it rain in Paris on 2026-11-01?',
      oracle: ORACLE,
      trading_close: t0 + 5_000,
      resolution_deadline: t0 + 15_000,
      dispute_window: 2_000,
    },
  });

  check(engineNow() < t0 + 100, `setup took too long: ${engineNow() - t0} ms past t0`);
  await sleep(t0 + 100 - engineNow());
  const { seq: request, batch: reqBatch } = await command(admin, 'req', ALICE, {
    SubmitRequest: {
      legs: [{ contract: t0, side: 'BuyYes', qty: 1_000, limit_price: 7_000 }],
      response_deadline: t0 + 1_500,
    },
  });
  check(balanceOf(reqBatch, ALICE)?.reserved > 0, `req: Alice's hold not in the batch: ${JSON.stringify(reqBatch.balances)}`);
  const quote = (price) => ({ SubmitQuote: { request, leg: 0, price, size: 1_000, expires_at: t0 + 10_000 } });
  const { seq: q1, batch: q1Batch } = await command(admin, 'q1', MM1, quote(6_200));
  check(balanceOf(q1Batch, MM1)?.reserved > 0, `q1: MM1's hold not in the batch`);
  const { seq: q2, batch: q2Batch } = await command(admin, 'q2', MM2, quote(6_000));
  check(balanceOf(q2Batch, MM2)?.reserved > 0, `q2: MM2's hold not in the batch`);

  // The engine presents the best quote at the response deadline (a Tick batch).
  const { frame: presentedBatch } = await admin.waitFor(
    'RequestPresented',
    (f) => f.type === 'batch' && f.events.some((e) => name(e) === 'RequestPresented' && body(e).request === request),
    { ms: TIMEOUT_MS + 1_500 },
  );
  const presented = body(presentedBatch.events.find((e) => name(e) === 'RequestPresented'));
  check(presented.picks.length === 1 && presented.picks[0].quote === q2, `presented ${JSON.stringify(presented.picks)}, expected MM2's quote ${q2}`);
  const { batch: acceptBatch } = await command(admin, 'accept', ALICE, { Accept: { request, picks: presented.picks } });
  const alice = balanceOf(acceptBatch, ALICE);
  check(alice?.escrowed === 600 * ONE_DOLLAR, `accept: Alice escrowed ${JSON.stringify(alice)}, expected $600`);

  // Events of this run, in arrival order.
  const ours = (e) => {
    const b = body(e);
    switch (name(e)) {
      case 'RequestOpened':
      case 'QuoteSubmitted':
      case 'RequestPresented':
      case 'RequestFilled':
        return b.request === request;
      case 'QuoteFilled':
        return b.quote === q1 || b.quote === q2;
      default:
        return false;
    }
  };
  const events = admin.frames.filter((f) => f.type === 'batch').flatMap((f) => f.events.filter(ours).map((e) => ({ seq: f.seq, e })));
  const expect = ['RequestOpened', 'QuoteSubmitted', 'QuoteSubmitted', 'RequestPresented', 'QuoteFilled', 'RequestFilled'];
  let next = 0;
  for (const { e } of events) if (next < expect.length && name(e) === expect[next]) next++;
  check(next === expect.length, `events out of order: got ${events.map(({ e }) => name(e)).join(', ')}; expected ${expect.join(', ')}`);

  // MM1's view: let anything still on its way arrive, then check every frame.
  await sleep(300);
  let own = 0;
  for (const f of mm1.frames.filter((f) => f.type === 'batch')) {
    for (const e of f.events) {
      const a = e.audience;
      check(a === 'Public' || a?.Account === MM1, `mm1 got an event addressed to ${JSON.stringify(a)}: ${JSON.stringify(e.event)} (seq ${f.seq})`);
      if (a?.Account === MM1) own++;
    }
    for (const b of f.balances) check(b.account === MM1, `mm1 got the balance of account ${b.account} (seq ${f.seq})`);
    check(f.origin === null || f.origin.actor === MM1, `mm1 got the origin of another account's command (seq ${f.seq})`);
  }
  const mm1Events = mm1.frames.filter((f) => f.type === 'batch').flatMap((f) => f.events.map(name));
  check(mm1Events.includes('QuoteSubmitted') && mm1Events.includes('QuoteLost'), `mm1 missed its own events: ${mm1Events.join(', ')}`);
  check(!mm1.frames.some((f) => f.type === 'result' || f.type === 'sent'), 'mm1 got a result or sent of a command it did not send');

  for (const r of results) {
    console.log(`  ${r.id.padEnd(10)} ${r.action.padEnd(14)} nonce ${String(r.nonce).padStart(4)}  seq ${String(r.seq).padStart(5)}  latency ${r.latency_us} us`);
  }
  for (const { seq, e } of events) console.log(`  seq ${String(seq).padStart(5)}  ${name(e)}`);
  console.log(`  alice after accept: free $${fmt(alice.free)}, reserved $${fmt(alice.reserved)}, escrowed $${fmt(alice.escrowed)}`);
  console.log(`  mm1 view: ${mm1.frames.filter((f) => f.type === 'batch').length} batches, ${own} events of its own, nothing of anyone else`);
  admin.close();
  mm1.close();
  console.log('GATEWAY SMOKE OK');
}

main().then(
  () => process.exit(0),
  (e) => {
    console.error(`GATEWAY SMOKE FAILED: ${e instanceof Failure ? e.message : e.stack}`);
    process.exit(1);
  },
);
