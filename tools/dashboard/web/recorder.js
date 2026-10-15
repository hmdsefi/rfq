// The recorder: gateway frames in, an ordered record of the run out. Plain script (inlined in the
// page, or `require`d by Node); no dependencies.
//
//   - `createRecorder()` keeps every frame with the time it was received here, merges the chunks of
//     a split batch by `seq`, and keeps the results, the engine statuses and the errors;
//   - `Scope` decides which events belong to one scenario run (a port of `rfq-demo`'s scope.rs);
//   - `containsInOrder` is the verdict (a port of `rfq_testkit::scenarios::contains_in_order`);
//   - `summarize`, `runPerf` and `aggregatePerf` are the latency numbers.
//
// Events keep the engine's serde shape: `{ audience, event }`, where `event` is `"Initialized"` or
// externally tagged, `{ "RequestOpened": { request, ... } }`. Nothing here computes money.
const RfqRecorder = (() => {
  // ---- events ------------------------------------------------------------------------------

  /** The variant name of an emitted event, `"RequestOpened"`. */
  function eventName(emitted) {
    const e = emitted.event;
    return typeof e === "string" ? e : Object.keys(e)[0];
  }

  /** The fields of an emitted event (`{}` for a variant without any). */
  function eventBody(emitted) {
    const e = emitted.event;
    return typeof e === "string" ? {} : Object.values(e)[0];
  }

  /**
   * Whether `emitted` is the event `want` asks for: its name, or for a rejection also
   * `CommandRejected(<Reason>)`, which pins the reason (testkit's `matches`).
   */
  function matches(emitted, want) {
    const name = eventName(emitted);
    if (name === "CommandRejected") return want === name || want === `${name}(${eventBody(emitted).reason})`;
    return want === name;
  }

  /**
   * Whether `events` contains the `expect`ed events in this order, other events interleaved
   * (testkit's `contains_in_order`). `{ ok: true }`, or `{ ok: false, error }` naming the first
   * expectation that was not met.
   */
  function containsInOrder(events, expect) {
    let from = 0;
    for (const want of expect) {
      let found = -1;
      for (let i = from; i < events.length; i++) {
        if (matches(events[i], want)) {
          found = i;
          break;
        }
      }
      if (found < 0) return { ok: false, error: `expected event ${want} (in order [${expect.join(", ")}])` };
      from = found + 1;
    }
    return { ok: true, error: null };
  }

  // ---- scope: what belongs to one run ---------------------------------------------------------

  /** Fixed by the node's genesis: the operator, the arbiter and the treasury are shared by every run. */
  const GENESIS_ROLES = [1, 2, 3];
  /** Room per run for the well-known scenario account ids (all below 100). */
  const ACCOUNTS_PER_RUN = 100;

  const originKey = (actor, nonce) => `${actor}:${nonce}`;

  /**
   * What belongs to one scenario run on a shared, long-lived engine (see rfq-demo's `scope.rs`).
   * The run's contract is `t0` (a multi-leg run lists extra markets instead, `t0 * 10 + k` for k in 1..=9),
   * its accounts are `t0 * 100 + id`, and its requests and quotes are the ones whose id is the `seq` of
   * one of its own commands. `owns` keeps exactly the events about those.
   */
  class Scope {
    constructor(t0) {
      this.key = t0;
      this.accounts = new Set();
      this.origins = new Set(); // "actor:nonce" of every command this run sent
      this.seqs = new Set(); // the seqs of the batches of those commands
      /** Names of events `owns` had no rule for (a new engine event: this file must be updated). */
      this.unclassified = new Set();
    }

    /** The run's own id for a scenario account (genesis roles keep theirs). */
    account(a) {
      if (GENESIS_ROLES.includes(a)) return a;
      if (!(a >= 0 && a < ACCOUNTS_PER_RUN)) throw new Error(`scenario account ${a} has no room in a run`);
      const own = this.key * ACCOUNTS_PER_RUN + a;
      if (!Number.isSafeInteger(own)) throw new Error(`account id ${own} is not a safe integer`);
      this.accounts.add(own);
      return own;
    }

    /** Records a command this run sent, so its batch counts as the run's. */
    sent(actor, nonce) {
      this.origins.add(originKey(actor, nonce));
    }

    /**
     * The run's own contract, or one of its extra markets: `t0 * 10 + k` for k in 1..=9 (what a script's
     * `ctx.contractN(k)` gives; `k = 0` is not one, so it can't be mistaken for the run's own). Rust:
     * `Scope::owns_contract`. Its `checked_mul` leaves a key that cannot be multiplied by 10 owning only its
     * own contract; here that is a key whose product is not a safe integer.
     */
    ownsContract(id) {
      if (id === this.key) return true;
      const base = this.key * 10;
      if (!Number.isSafeInteger(base)) return false;
      const k = id - base;
      return k >= 1 && k <= 9;
    }

    /**
     * Whether event `e` of `batch` is about this run's objects. Call it for every event of every
     * batch, in arrival order: the batch of one of the run's commands adds its `seq`.
     */
    owns(batch, e) {
      const own = batch.origin != null && this.origins.has(originKey(batch.origin.actor, batch.origin.nonce));
      if (own) this.seqs.add(batch.seq);
      const account = (a) => this.accounts.has(a);
      const object = (id) => this.seqs.has(id);
      const b = eventBody(e);
      switch (eventName(e)) {
        case "Initialized":
          return false;
        // Only the command's own handling rejects; timers never do.
        case "CommandRejected":
          return own;
        case "Deposited":
        case "Withdrawn":
          return account(b.account);
        case "ContractListed":
        case "ContractClosed":
        case "OutcomeProposed":
        case "Disputed":
        case "Ruled":
        case "DisputeBondReturned":
        case "ContractFinalized":
          return this.ownsContract(b.contract);
        case "RequestOpened":
        case "RequestCancelled":
        case "RequestExpired":
        case "RequestPresented":
        case "RequestRejected":
        case "RequestFilled":
          return object(b.request);
        case "QuoteSubmitted":
        case "QuoteCancelled":
        case "QuoteExpired":
        case "QuoteLost":
        case "QuoteSelected":
        case "QuoteReleased":
        case "QuoteFilled":
          return object(b.quote);
        case "QuoteReplaced":
          return object(b.old) || object(b.new);
        case "PositionSettled":
          return account(b.yes_holder) || account(b.no_holder);
        default:
          this.unclassified.add(eventName(e));
          return false;
      }
    }
  }

  // ---- latency numbers ------------------------------------------------------------------------

  /**
   * The nearest-rank percentile `p` (0-100) of `sorted` (ascending): the smallest sample with at
   * least p% of the samples at or below it. `null` for no samples.
   */
  function percentile(sorted, p) {
    if (sorted.length === 0) return null;
    const rank = Math.max(1, Math.ceil((p / 100) * sorted.length));
    return sorted[Math.min(rank, sorted.length) - 1];
  }

  /** `{ n, p50, p95, max }` of `samples` (any order). */
  function summarize(samples) {
    const sorted = Float64Array.from(samples).sort();
    return {
      n: sorted.length,
      p50: percentile(sorted, 50),
      p95: percentile(sorted, 95),
      max: sorted.length ? sorted[sorted.length - 1] : null,
    };
  }

  /**
   * One run's numbers from its commands `{ latency_us, rtt_ms }`: the gateway's latency (from the
   * `result`) and the round trip measured here, each as p50, p95 and max; commands per second over
   * the run's wall time.
   */
  function runPerf(commands, wallMs) {
    return {
      commands: commands.length,
      latency_us: summarize(commands.map((c) => c.latency_us)),
      rtt_ms: summarize(commands.map((c) => c.rtt_ms)),
      wall_ms: wallMs,
      commands_per_s: wallMs > 0 ? commands.length / (wallMs / 1000) : null,
    };
  }

  /** Many runs together: every command's sample counts once, and the rate is over `wallMs`, the whole window. */
  function aggregatePerf(runs, wallMs) {
    const commands = runs.flatMap((r) => r.commands);
    return { runs: runs.length, ...runPerf(commands, wallMs) };
  }

  // ---- the recorder -----------------------------------------------------------------------

  /**
   * Keeps what the gateway sends. `onFrame(frame, meta)` takes every frame (`meta.at` is `Date.now()`
   * and `meta.t` is `performance.now()` at receipt; they are taken now if `meta` is left out), or
   * `attach(gateway)` feeds it from a `RfqGateway` connection.
   *
   * **Epochs.** Seqs only grow on one journal. A batch whose seq is below the newest one's is from a
   * new journal (`docker compose down -v`): it starts a new epoch instead of being merged into an
   * unrelated old batch. `batches`, `order` and `batchList()` are the current epoch's (results and
   * origins name the current engine's seqs); `allBatches()` keeps every epoch.
   */
  function createRecorder() {
    const rec = {
      /** Every frame, in arrival order: `{ n, at, t, frame }`. */
      frames: [],
      /** The current epoch, from 0: how many times the seqs started again. */
      epoch: 0,
      /** The current epoch's batches, merged by `seq`, by seq. */
      batches: new Map(),
      /** The seqs of `batches`, in the order their first chunk arrived. */
      order: [],
      /** Every `result`: `{ n, at, t, id, seq, latency_us }`. */
      results: [],
      /** Every `status` and `welcome`: `{ n, at, engine, last_seq, kind }`. */
      statuses: [],
      /** Every `error`: `{ n, at, id, error }`. */
      errors: [],
      onFrame,
      attach,
      /** The current epoch's merged batches in arrival order. */
      batchList: () => rec.order.map((seq) => rec.batches.get(seq)),
      /** Every merged batch of every epoch, in arrival order (each carries its `epoch`). */
      allBatches: () => all.slice(),
      clear,
    };
    const all = [];
    let newest = null; // the merged batch whose chunk arrived last

    function onFrame(frame, meta = {}) {
      const at = meta.at ?? Date.now();
      const t = meta.t ?? performance.now();
      const n = meta.n ?? rec.frames.length + 1;
      rec.frames.push({ n, at, t, frame });
      switch (frame.type) {
        case "batch":
          return mergeBatch(frame, n, at, t);
        case "result":
          rec.results.push({ n, at, t, id: frame.id, seq: frame.seq, latency_us: frame.latency_us });
          break;
        case "status":
          rec.statuses.push({ n, at, engine: frame.engine, last_seq: frame.last_seq ?? null, kind: "status" });
          break;
        case "welcome":
          rec.statuses.push({ n, at, engine: frame.engine, last_seq: null, kind: "welcome" });
          break;
        case "error":
          rec.errors.push({ n, at, id: frame.id ?? null, error: frame.error });
          break;
        default:
      }
      return null;
    }

    /**
     * A batch too big for one message arrives as chunks with the same `seq`, time, kind and origin
     * (`split_batch`), one after the other. Their events concatenate in arrival order; the balances
     * travel once, on the last chunk (which may carry nothing else), so they are the last chunk's.
     * Only the newest batch can take a chunk: an older seq seen again is a new epoch.
     */
    function mergeBatch(frame, n, at, t) {
      if (newest && frame.seq === newest.seq && newest.epoch === rec.epoch) {
        newest.events.push(...frame.events);
        newest.balances = frame.balances ?? [];
        newest.chunks++;
        newest.lastN = n;
        newest.lastAt = at;
        return newest;
      }
      if (newest && frame.seq < newest.seq) {
        rec.epoch++;
        rec.batches = new Map();
        rec.order = [];
      }
      const merged = {
        seq: frame.seq,
        epoch: rec.epoch,
        time: frame.time,
        kind: frame.kind,
        origin: frame.origin ?? null,
        events: [...frame.events],
        balances: frame.balances ?? [],
        chunks: 1,
        firstN: n,
        lastN: n,
        firstAt: at,
        lastAt: at,
        firstT: t,
      };
      rec.batches.set(frame.seq, merged);
      rec.order.push(frame.seq);
      all.push(merged);
      newest = merged;
      return merged;
    }

    function attach(gateway) {
      return gateway.on("*", onFrame);
    }

    function clear() {
      rec.frames.length = 0;
      rec.epoch = 0;
      rec.batches = new Map();
      rec.order = [];
      all.length = 0;
      newest = null;
      rec.results.length = 0;
      rec.statuses.length = 0;
      rec.errors.length = 0;
    }

    return rec;
  }

  /**
   * Feeds the frames of one gateway connection to the recorder of the view they were sent to:
   * `recorderFor(view)` (`"admin"` or `{ account }`, as the `welcome` echoed it) returns a recorder,
   * or null to drop them. The view is the one the last `welcome` on the current socket confirmed.
   * Until a socket's first `welcome` its view is the gateway's default, `admin` (gateway.md §3), so
   * those frames are never a participant's and are dropped; so is everything after a dropped
   * connection until the new socket's `welcome`. The `welcome` itself goes to its view's recorder.
   * Returns the function that detaches it.
   */
  function attachViews(gateway, recorderFor) {
    let target = null; // the recorder of the confirmed view, or null
    const offs = [
      gateway.on("conn", (c) => {
        if (!c.up) target = null;
      }),
      gateway.on("*", (frame, meta) => {
        if (frame.type === "welcome") target = recorderFor(frame.view) ?? null;
        target?.onFrame(frame, meta);
      }),
    ];
    return () => offs.forEach((off) => off());
  }

  return {
    createRecorder,
    attachViews,
    Scope,
    GENESIS_ROLES,
    ACCOUNTS_PER_RUN,
    containsInOrder,
    matches,
    eventName,
    eventBody,
    percentile,
    summarize,
    runPerf,
    aggregatePerf,
  };
})();
if (typeof module !== "undefined") module.exports = RfqRecorder;
