// The 12 scenarios as data, and the driver that plays one as its participants through the gateway:
// dashboard -> WebSocket -> rfq-gateway -> Aeron -> rfq-node. Plain script (inlined in the page, or
// `require`d by Node); it needs recorder.js (and, in the page, that script comes first).
//
//   const driver = RfqScenarios.createDriver(gateway);              // gateway: RfqGateway.connect(...)
//   const { runs, perf } = await driver.runMany(["happy_path"], 20, 4);
//
// A step is `{ at, label, actor, action: (ctx) => Action, nonce? }` (a command, sent when the engine's
// clock reaches `t0 + at`) or `{ at, wait: true }` (a former `tick`: the engine fires its own timers,
// so the driver only waits). The scripts name the logical accounts (ALICE = 10, ...); the driver maps
// them to the run's own ids, as rfq-demo's `scope.rs` does. Money is integer micro-USD and prices are
// bps, as the engine has them: nothing here computes money.
const RfqScenarios = (() => {
  const R = typeof require === "function" ? require("./recorder.js") : RfqRecorder;

  /** Lead time, in ms, between computing a run's `t0` and its first step (rfq-demo's START_MARGIN). */
  const START_MARGIN_MS = 500;
  /** A wait step sleeps until `t0 + at` plus this, so the timers due at `at` have fired (and been heard). */
  const WAIT_GRACE_MS = 300;
  /** After a run's last step, how long its timer-driven events may take to arrive (rfq-demo's GRACE). */
  const GRACE_MS = 3_000;
  /** How long a run waits for the engine to come back after an unknown outcome, or to be up at the start. */
  const ENGINE_WAIT_MS = 60_000;
  /** How many times one command's unknown outcome is retried. */
  const MAX_RETRIES = 3;

  const ONE_DOLLAR = 1_000_000;
  const dollars = (x) => x * ONE_DOLLAR;
  /** A price in bps, 1..=9999 (the engine refuses others). */
  const price = (bps) => {
    if (!Number.isInteger(bps) || bps < 1 || bps > 9_999) throw new RangeError(`price ${bps} is not 1..=9999 bps`);
    return bps;
  };

  /** The well-known accounts (`rfq_testkit::accounts`). OPERATOR, ARBITER and TREASURY are the node's genesis roles. */
  const OPERATOR = 1;
  const ARBITER = 2;
  const TREASURY = 3;
  const ORACLE = 4;
  const ALICE = 10;
  const MM1 = 20;
  const MM2 = 21;
  const CAROL = 30;
  const ACCOUNT_NAMES = {
    [OPERATOR]: "operator",
    [ARBITER]: "arbiter",
    [TREASURY]: "treasury",
    [ORACLE]: "oracle",
    [ALICE]: "alice",
    [MM1]: "mm1",
    [MM2]: "mm2",
    [CAROL]: "carol",
  };

  // ---- ctx: what a step may ask (testkit's `Ctx`) --------------------------------------------

  class Ctx {
    /** `account(id)` maps a scenario account to the run's own (a `Scope`'s); the identity when left out. */
    constructor(t0, account = (a) => a) {
      this.t0 = t0;
      this.labels = new Map();
      this.presented = new Map(); // request id -> picks
      this.account = account;
    }

    /** `t0 + offset`, in engine ms. */
    at(offset) {
      return this.t0 + offset;
    }

    /** One contract per run, keyed by its start time so reruns never collide. */
    contract() {
      return this.t0;
    }

    recordLabel(label, seq) {
      this.labels.set(label, seq);
    }

    seq(label) {
      if (!this.labels.has(label)) throw new Error(`label ${label} has no seq yet`);
      return this.labels.get(label);
    }

    /** The id of the request the command labelled `label` made: the `seq` of its batch. */
    request(label) {
      return this.seq(label);
    }

    /** The id of the quote the command labelled `label` made. */
    quote(label) {
      return this.seq(label);
    }

    /** The picks the engine presented for `request`; none yet if it has not. */
    picks(request) {
      return this.presented.get(request) ?? [];
    }

    /** Notes what the engine said (every batch, the run's or not, as rfq-demo does): the presented picks. */
    observe(events) {
      for (const e of events) {
        if (R.eventName(e) === "RequestPresented") {
          const { request, picks } = R.eventBody(e);
          this.presented.set(request, picks);
        }
      }
    }
  }

  /**
   * `action` with every account id it carries mapped through `f`. Only `Deposit` and `ListContract`
   * carry any (rfq-demo's `remap`); an action this does not know is an error, so that a new one must
   * be looked at here.
   */
  function remap(action, f) {
    const [name, body] = Object.entries(action)[0];
    switch (name) {
      case "Deposit":
        return { Deposit: { ...body, to: f(body.to) } };
      case "ListContract":
        return { ListContract: { ...body, oracle: f(body.oracle) } };
      case "Withdraw":
      case "SubmitRequest":
      case "CancelRequest":
      case "SubmitQuote":
      case "CancelQuote":
      case "Accept":
      case "Reject":
      case "ProposeOutcome":
      case "Dispute":
      case "Rule":
        return action;
      default:
        throw new Error(`remap does not know the action ${name}`);
    }
  }

  /**
   * What to send for a command step: the actor and action in the run's accounts, and the nonce for the
   * attack scripts. `lastNonce(actor)` is the nonce of the actor's last ordinary command (the gateway
   * assigned it), which `"reuse"` sends again (a replay) and `"skip"` jumps over (`last + 2`, a gap).
   * `nonce` is left out for an ordinary step: the gateway assigns the actor's next one.
   */
  function resolveStep(step, ctx, lastNonce) {
    const actor = ctx.account(step.actor);
    const action = remap(step.action(ctx), ctx.account);
    if (step.nonce === undefined) return { actor, action };
    const last = lastNonce(actor);
    if (last === undefined) throw new Error(`nonce "${step.nonce}" needs an earlier command of account ${actor}`);
    if (step.nonce === "reuse") return { actor, action, nonce: last };
    if (step.nonce === "skip") return { actor, action, nonce: last + 2 };
    throw new Error(`unknown nonce rule ${step.nonce}`);
  }

  // ---- the scripts -----------------------------------------------------------------------------

  /** A command step. `nonce` is `"reuse"` or `"skip"` for the attack scripts. */
  const send = (at, label, actor, action, nonce) => (nonce === undefined ? { at, label, actor, action } : { at, label, actor, action, nonce });
  /** A step that only waits (testkit's `Tick`): the engine fires its own timers. */
  const wait = (at) => ({ at, wait: true });

  /**
   * Funds everyone and lists the run's contract (closes at +5 s, hard deadline at +15 s).
   * Rust: `rfq_testkit::scenarios::setup`; the dashboard's `presets::setup` is the same.
   */
  function setup() {
    const steps = [
      ["fund_alice", ALICE],
      ["fund_mm1", MM1],
      ["fund_mm2", MM2],
      ["fund_carol", CAROL],
    ].map(([label, to]) => send(0, label, OPERATOR, () => ({ Deposit: { to, amount: dollars(1_000) } })));
    steps.push(
      send(0, "list", OPERATOR, (c) => ({
        ListContract: {
          id: c.contract(),
          description: "Will it rain in Paris on 2026-11-01?",
          oracle: ORACLE,
          trading_close: c.at(5_000),
          resolution_deadline: c.at(15_000),
          dispute_window: 2_000,
        },
      })),
    );
    return steps;
  }

  /** Alice asks for 1000 YES with a sealed 0.70 limit; quotes due at +1.5 s. Rust: the `req` step of `quoted()` (`presets::request`). */
  const request = (at) =>
    send(at, "req", ALICE, (c) => ({
      SubmitRequest: {
        legs: [{ contract: c.contract(), side: "BuyYes", qty: 1_000, limit_price: price(7_000) }],
        response_deadline: c.at(1_500),
      },
    }));

  /** A quote on Alice's request for 1000 at `bps`. Rust: the `q1` and `q2` steps of `quoted()` (`presets::quote`). */
  const quote = (at, label, mm, bps) =>
    send(at, label, mm, (c) => ({
      SubmitQuote: { request: c.request("req"), leg: 0, price: price(bps), size: 1_000, expires_at: c.at(10_000) },
    }));

  /** Request 1000 YES (limit 0.70), quotes at 0.62 and 0.60, presented at +1.5 s. Rust: `scenarios::quoted`. */
  function quoted() {
    return [...setup(), request(100), quote(300, "q1", MM1, 6_200), quote(400, "q2", MM2, 6_000), wait(1_600)];
  }

  /** Alice accepts what the engine presented. Rust: `scenarios::accept` (`presets::accept`). */
  const accept = (at, label = "accept") =>
    send(at, label, ALICE, (c) => ({ Accept: { request: c.request("req"), picks: c.picks(c.request("req")) } }));

  /** Quoted, then accepted at +2 s. Rust: `scenarios::filled`. */
  const filled = () => [...quoted(), accept(2_000)];

  /** The oracle proposes YES. Rust: the `propose` step of `scenarios::all` (happy, disputed). */
  const propose = (at) => send(at, "propose", ORACLE, (c) => ({ ProposeOutcome: { contract: c.contract(), outcome: "Yes" } }));

  const BRIEF_SUMMARY = {
    happy_path: "Two MMs quote, the best price is presented, Alice accepts, the oracle proposes YES and the contract settles.",
    mm_cannot_withdraw_after_selection:
      "MM2 tries to cancel its selected quote inside the accept window: rejected, the quote is firm, Alice's accept fills.",
    no_quotes_expire: "Nobody quotes: at the deadline the request expires and Alice's hold is released.",
    requester_lapses: "Alice never answers: at accept_by the request is rejected and every hold is released.",
    disputed_ruled_invalid:
      "Carol disputes the proposed outcome with a bond; the arbiter rules INVALID and the positions are refunded.",
    oracle_silent_hard_deadline: "The oracle never proposes: at the hard deadline the contract resolves INVALID and refunds everyone.",
  };

  // Each script below names its Rust source. `expect` is checked by `containsInOrder` (testkit's
  // `contains_in_order`): the names in this order, other events interleaved, and
  // `CommandRejected(<Reason>)` pins a rejection's reason.
  const scenarios = [
    // Rust: `rfq_testkit::scenarios::all()`, `happy`.
    {
      name: "happy_path",
      kind: "brief",
      steps: [...filled(), wait(5_100), propose(5_200), wait(7_300)],
      expect: [
        "RequestOpened",
        "QuoteSubmitted",
        "QuoteSubmitted",
        "QuoteLost",
        "QuoteSelected",
        "RequestPresented",
        "QuoteFilled",
        "RequestFilled",
        "ContractClosed",
        "OutcomeProposed",
        "ContractFinalized",
        "PositionSettled",
      ],
    },
    // Rust: `rfq_testkit::scenarios::all()`, `withdraw`. The selected quote is firm (RFC P5): MM2's
    // cancel inside the window is rejected and the accept fills.
    {
      name: "mm_cannot_withdraw_after_selection",
      kind: "brief",
      steps: [...quoted(), send(1_800, "cancel", MM2, (c) => ({ CancelQuote: { quote: c.quote("q2") } })), accept(1_900)],
      expect: ["RequestPresented", "CommandRejected(InvalidState)", "QuoteFilled", "RequestFilled"],
    },
    // Rust: `rfq_testkit::scenarios::all()`, `no_quotes`.
    {
      name: "no_quotes_expire",
      kind: "brief",
      steps: [...setup(), request(100), wait(1_600)],
      expect: ["RequestOpened", "RequestExpired"],
    },
    // Rust: `rfq_testkit::scenarios::all()`, `lapse`.
    {
      name: "requester_lapses",
      kind: "brief",
      steps: [...quoted(), wait(3_600)],
      expect: ["RequestPresented", "QuoteReleased", "RequestRejected"],
    },
    // Rust: `rfq_testkit::scenarios::all()`, `disputed`.
    {
      name: "disputed_ruled_invalid",
      kind: "brief",
      steps: [
        ...filled(),
        wait(5_100),
        propose(5_200),
        send(5_500, "dispute", CAROL, (c) => ({ Dispute: { contract: c.contract() } })),
        send(6_000, "rule", ARBITER, (c) => ({ Rule: { contract: c.contract(), outcome: "Invalid" } })),
      ],
      expect: ["RequestFilled", "OutcomeProposed", "Disputed", "Ruled", "ContractFinalized", "PositionSettled"],
    },
    // Rust: `rfq_testkit::scenarios::all()`, `silent`.
    {
      name: "oracle_silent_hard_deadline",
      kind: "brief",
      steps: [...filled(), wait(15_100)],
      expect: ["RequestFilled", "ContractClosed", "ContractFinalized", "PositionSettled"],
    },
    // Rust: `tools/dashboard/src/presets.rs`, `attacks()`, `wrong_price`. Alice accepts at 0.61
    // instead of the presented 0.60; the quote stays firm and her real accept fills.
    {
      name: "attack_accept_wrong_price",
      kind: "attack",
      steps: [
        ...quoted(),
        send(1_800, "bad_accept", ALICE, (c) => {
          const req = c.request("req");
          return { Accept: { request: req, picks: c.picks(req).map((p) => ({ ...p, price: price(6_100) })) } };
        }),
        accept(1_900),
      ],
      expect: ["RequestPresented", "CommandRejected(Mismatch)", "QuoteFilled", "RequestFilled"],
    },
    // Rust: `presets.rs`, `attacks()`, `double_accept`. The second accept is rejected; escrow happens once.
    {
      name: "attack_double_accept",
      kind: "attack",
      steps: [...quoted(), accept(1_800), accept(1_900, "accept_again")],
      expect: ["RequestFilled", "CommandRejected(InvalidState)"],
    },
    // Rust: `presets.rs`, `attacks()`, `replayed_nonce`. MM1 replays its used nonce, then skips one:
    // Duplicate, then NonceGap, and neither consumes anything; its next real command still works.
    {
      name: "attack_replayed_nonce",
      kind: "attack",
      steps: [
        ...setup(),
        request(100),
        quote(300, "q1", MM1, 6_200),
        send(
          400,
          "q1_replayed",
          MM1,
          (c) => ({ SubmitQuote: { request: c.request("req"), leg: 0, price: price(5_000), size: 1_000, expires_at: c.at(10_000) } }),
          "reuse",
        ),
        send(500, "q1_gap", MM1, (c) => ({ CancelQuote: { quote: c.quote("q1") } }), "skip"),
        send(600, "q1_cancel", MM1, (c) => ({ CancelQuote: { quote: c.quote("q1") } })),
      ],
      expect: ["QuoteSubmitted", "CommandRejected(Duplicate)", "CommandRejected(NonceGap)", "QuoteCancelled"],
    },
    // Rust: `presets.rs`, `attacks()`, `over_limit`. MM2 is the only quote, at 0.75, above Alice's
    // sealed 0.70 limit: accepted as Active but never eligible; at the deadline it is Lost.
    {
      name: "attack_over_limit_quote",
      kind: "attack",
      steps: [...setup(), request(100), quote(300, "q_over", MM2, 7_500), wait(1_600)],
      expect: ["QuoteSubmitted", "QuoteLost", "RequestExpired"],
    },
    // Rust: `presets.rs`, `attacks()`, `self_trade`. Alice quotes on her own request.
    {
      name: "attack_self_trade",
      kind: "attack",
      steps: [...setup(), request(100), quote(300, "self_quote", ALICE, 6_000)],
      expect: ["RequestOpened", "CommandRejected(SelfTrade)"],
    },
    // Rust: `presets.rs`, `attacks()`, `cancel_others`. Carol tries to cancel MM1's quote.
    {
      name: "attack_cancel_others_quote",
      kind: "attack",
      steps: [
        ...setup(),
        request(100),
        quote(300, "q1", MM1, 6_200),
        send(400, "carol_cancel", CAROL, (c) => ({ CancelQuote: { quote: c.quote("q1") } })),
      ],
      expect: ["QuoteSubmitted", "CommandRejected(Unauthorized)"],
    },
  ];

  const ATTACK_SUMMARY = {
    attack_accept_wrong_price:
      "Alice accepts at 0.61 instead of the presented 0.60: rejected (Mismatch); the quote stays firm and her real accept fills.",
    attack_double_accept: "Alice accepts twice: the second is rejected (InvalidState); escrow happens exactly once.",
    attack_replayed_nonce:
      "MM1 replays a used nonce, then skips one: Duplicate, then NonceGap, and neither consumes anything; its next real command still works.",
    attack_over_limit_quote:
      "MM2 is the only quote, at 0.75, above Alice's sealed 0.70 limit: accepted as Active but never eligible. At the deadline it's Lost and the request expires; every hold is released.",
    attack_self_trade: "Alice quotes on her own request: rejected (SelfTrade).",
    attack_cancel_others_quote: "Carol tries to cancel MM1's quote: rejected (Unauthorized); the quote stays Active.",
  };
  for (const s of scenarios) s.summary = BRIEF_SUMMARY[s.name] ?? ATTACK_SUMMARY[s.name];

  const byName = (name) => scenarios.find((s) => s.name === name);

  // ---- the driver --------------------------------------------------------------------------------

  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const actionName = (action) => Object.keys(action)[0];
  /** The reason of the first rejection among `events`, or null. */
  const rejectionOf = (events) => {
    const rejected = events.find((e) => R.eventName(e) === "CommandRejected");
    return rejected ? R.eventBody(rejected).reason : null;
  };
  /** Whether `events` say that the nonce was not consumed (Duplicate or NonceGap, the engine's rule). */
  const unconsumed = (events) =>
    events.some((e) => R.eventName(e) === "CommandRejected" && ["Duplicate", "NonceGap"].includes(R.eventBody(e).reason));

  /** Connections that have a driver: one driver per connection (and so per page) until `dispose()`. */
  const DRIVEN = new WeakSet();

  /** What a run throws when the presenter stops it (or the driver is disposed). */
  class Stopped extends Error {
    constructor() {
      super("stopped by the presenter: nothing more was sent; the engine finishes what was already sent");
      this.name = "Stopped";
    }
  }

  /**
   * Plays scenarios through `gateway` (an `RfqGateway.connect` connection, in the `admin` view: the
   * verdict needs every event, sealed ones too). One driver per connection: it listens to the frames,
   * and a second `createDriver` on the same connection is refused until the first is `dispose()`d.
   *
   * `opts`: `recorder` (default: a new one, attached to `gateway`; do not attach it yourself),
   * `onLog(run, line)` for each line of a run's log as it is written, `onRunStart(run)`, `onRunEnd(run)`,
   * `onStep(run)` when a run moves to its next step (`run.step` of `run.steps`); and the timings, for
   * tests: `startMarginMs`, `graceMs`, `engineWaitMs` (defaults above).
   *
   * Besides `run` and `runMany`: `stop()` abandons the runs in progress (they send nothing more; the
   * engine finishes what was already sent) and keeps the driver usable; `dispose()` stops and lets go
   * of the connection; `commandAt(seq)` is the command the driver sent that made batch `seq`.
   */
  function createDriver(gateway, opts = {}) {
    if (DRIVEN.has(gateway)) throw new Error("this gateway connection already has a driver: dispose() it first");
    DRIVEN.add(gateway);
    const startMargin = opts.startMarginMs ?? START_MARGIN_MS;
    const grace = opts.graceMs ?? GRACE_MS;
    const engineWait = opts.engineWaitMs ?? ENGINE_WAIT_MS;
    const recorder = opts.recorder ?? R.createRecorder();
    const active = new Set();
    /** Every command id this driver sent: `{ id, run, step, actor, action, nonce }` (nonce once `sent`). */
    const sentById = new Map();
    /** seq -> the command whose `result` named that batch. */
    const bySeq = new Map();
    let lastT0 = 0;
    let runNumber = 0;
    let disposed = false;
    let measuring = null; // the clock measurement in progress
    // A Stop ends a generation: runs and run sets of an older one stop at their next await.
    let generation = 0;
    let stopSignal = signal();

    function signal() {
      let fire;
      const promise = new Promise((resolve) => (fire = resolve));
      return { promise, fire };
    }

    const offs = [
      recorder.attach(gateway),
      // One listener each for all the runs: a `sent` frame tells its run the nonce of its command (so
      // that the batch of that command counts as the run's), and every batch goes to every run's scope.
      gateway.on("sent", (frame) => {
        const cmd = sentById.get(frame.id);
        if (cmd) cmd.nonce = frame.nonce;
        for (const run of active) if (run.ids.has(frame.id)) run.scope.sent(frame.actor, frame.nonce);
      }),
      gateway.on("batch", (frame) => {
        for (const run of active) run.onBatch(frame);
      }),
      // A result (a late one too, after an unknown outcome) names the batch its command made.
      gateway.on("result", (frame) => {
        const cmd = sentById.get(frame.id);
        if (cmd) bySeq.set(frame.seq, cmd);
      }),
    ];

    const engineNow = () => gateway.engineNow();

    /** Throws `Stopped` if the run's generation was stopped. */
    function check(run) {
      if (run.generation !== generation) throw new Stopped();
    }

    /** `promise`, or `Stopped` as soon as the run is stopped. */
    function orStop(run, promise) {
      check(run);
      return Promise.race([promise, run.stopped.then(() => Promise.reject(new Stopped()))]);
    }

    async function sleepUntil(run, engineMs) {
      for (;;) {
        check(run);
        const ms = engineMs - engineNow();
        if (ms <= 0) return;
        // In pieces: the clock offset is measured again on a reconnect, and a Stop ends the wait.
        await sleep(Math.min(ms, 100));
      }
    }

    /** A new `t0`: the engine's now plus the lead, and never one a run already has (so 1 ms apart at least). */
    function allocT0() {
      const t0 = Math.max(Math.ceil(engineNow()) + startMargin, lastT0 + 1);
      lastT0 = t0;
      return t0;
    }

    /**
     * Before runs: the admin view, the engine up, and the clocks measured against each other afresh
     * (they drift). Only while no run is active: a run in progress schedules its steps by the offset,
     * so it must not move under it.
     */
    async function prepare() {
      if (disposed) throw new Error("this driver is disposed");
      if (gateway.view !== "admin") {
        throw new Error("scenarios need the admin view: the verdict reads every event, sealed ones too");
      }
      if (gateway.engine !== "up") await gateway.untilEngineUp({ timeoutMs: engineWait });
      if (measuring) await measuring;
      else if (active.size === 0) {
        measuring = gateway.syncClock(3).finally(() => (measuring = null));
        await measuring;
      }
    }

    async function runOne(which, gen, t0 = allocT0()) {
      const scenario = typeof which === "string" ? byName(which) : which;
      if (!scenario) throw new Error(`no scenario named ${which}; the scenarios are ${scenarios.map((s) => s.name).join(", ")}`);
      const run = newRun(scenario, t0, gen);
      active.add(run);
      opts.onRunStart?.(run.result);
      try {
        await play(run);
      } catch (e) {
        run.fail(e);
      } finally {
        active.delete(run);
      }
      run.finish();
      opts.onRunEnd?.(run.result);
      return run.result;
    }

    function newRun(scenario, t0, gen) {
      const scope = new R.Scope(t0);
      const accounts = {};
      const ctx = new Ctx(t0, (a) => {
        const own = scope.account(a);
        accounts[ACCOUNT_NAMES[a] ?? a] = own;
        return own;
      });
      const key = `r${++runNumber}`;
      const startedAt = Date.now();
      const result = {
        id: key,
        name: scenario.name,
        kind: scenario.kind,
        t0,
        contract: t0,
        accounts,
        expect: scenario.expect,
        pass: false,
        stopped: false,
        detail: null,
        error: null,
        step: 0,
        steps: scenario.steps.length,
        own: [], // the run's own events: { seq, time, kind, audience, event }
        batches: [], // the seqs of the run's batches, in arrival order: its commands' (retries too) and those with its own events
        labels: {},
        commands: [], // every command with its numbers; see `command`
        recoveries: [],
        log: [],
        startedAt,
        wall_ms: 0,
        perf: null,
        done: false,
      };
      const run = {
        key,
        scenario,
        scope,
        ctx,
        result,
        generation: gen,
        stopped: stopSignal.promise,
        ids: new Set(), // the ids of the run's commands, to tell its `sent` frames from another run's
        lastNonce: new Map(), // actor -> nonce of its last ordinary command
        /**
         * Origins (`actor:nonce`) whose unknown outcome was retried. Of the two copies, the engine applies
         * the first and rejects the other as Duplicate: that Duplicate is a recovery artifact, never the
         * run's own event (so it cannot satisfy an expect), whichever copy it was. Armed once per retry.
         */
        artifacts: new Set(),
        commandCount: 0,
        failure: null,
        log(line) {
          result.log.push(line);
          opts.onLog?.(result, line);
        },
        onBatch(frame) {
          ctx.observe(frame.events);
          const origin = frame.origin == null ? null : `${frame.origin.actor}:${frame.origin.nonce}`;
          let mine = origin != null && scope.origins.has(origin);
          if (origin != null && run.artifacts.has(origin) && unconsumed(frame.events)) {
            run.artifacts.delete(origin);
            for (const e of frame.events) scope.owns(frame, e); // keeps the scope's bookkeeping; owns nothing new
          } else {
            for (const e of frame.events) {
              if (scope.owns(frame, e)) {
                mine = true;
                result.own.push({ seq: frame.seq, time: frame.time, kind: frame.kind, audience: e.audience, event: e.event });
              }
            }
          }
          if (mine && result.batches.at(-1) !== frame.seq) result.batches.push(frame.seq);
        },
        fail(e) {
          run.failure ??= e instanceof Error ? e : new Error(String(e));
          result.error = run.failure.message;
          if (run.failure instanceof Stopped) result.stopped = true;
        },
        finish() {
          const v = R.containsInOrder(result.own, scenario.expect);
          const unclassified = [...scope.unclassified];
          if (!run.failure && unclassified.length) {
            run.fail(new Error(`the engine emitted ${unclassified.join(", ")}, which the scope has no rule for (update recorder.js)`));
          }
          result.pass = !run.failure && v.ok;
          result.detail = run.failure ? run.failure.message : v.error;
          result.labels = Object.fromEntries(ctx.labels);
          result.wall_ms = Date.now() - startedAt;
          result.perf = R.runPerf(
            result.commands.filter((c) => !c.recovered),
            result.wall_ms,
          );
          result.done = true;
          run.log(result.stopped ? "STOPPED" : result.pass ? "PASS" : `FAIL: ${result.detail}`);
        },
      };
      return run;
    }

    /** Plays every step at its time, then waits for the timer-driven events, as rfq-demo's `scenario`. */
    async function play(run) {
      const { scenario, ctx, result } = run;
      const t0 = ctx.t0;
      run.log(`${scenario.name}: t0=${t0} (engine now + ${startMargin} ms), contract ${t0}`);
      for (const [i, step] of scenario.steps.entries()) {
        check(run);
        result.step = i + 1;
        opts.onStep?.(result);
        if (step.wait) {
          await sleepUntil(run, t0 + step.at + WAIT_GRACE_MS);
          continue;
        }
        await sleepUntil(run, t0 + step.at);
        await runStep(run, step);
      }
      // The events the engine's timers produce after the last step.
      const deadline = Date.now() + grace;
      while (!R.containsInOrder(result.own, scenario.expect).ok && Date.now() < deadline) {
        check(run);
        await sleep(20);
      }
    }

    /** Sends one command for `run` (step `label`) and remembers it, so `commandAt` can name its batch. */
    function send(run, label, id, cmd) {
      run.ids.add(id);
      sentById.set(id, { id, run: run.key, step: label, actor: cmd.actor, action: cmd.action, nonce: cmd.nonce ?? null });
      return orStop(run, gateway.command({ id, ...cmd }));
    }

    /** Sends one command step and waits for its `result`; records its seq as the step's label. */
    async function runStep(run, step) {
      const { ctx, result } = run;
      let cmd;
      try {
        cmd = resolveStep(step, ctx, (actor) => run.lastNonce.get(actor));
      } catch (e) {
        throw new Error(`step ${step.label} (+${step.at} ms): ${e.message}`);
      }
      const what = `${step.label} (${ACCOUNT_NAMES[step.actor] ?? step.actor} ${actionName(cmd.action)})`;
      const id = `${run.key}.${++run.commandCount}`;
      // A replay or a gap the script sends on purpose: its rejection is the step's answer, never an artifact.
      if (cmd.nonce !== undefined) run.artifacts.delete(`${cmd.actor}:${cmd.nonce}`);
      const late_ms = Math.round(engineNow() - (ctx.t0 + step.at));
      let answer;
      let recovered = false;
      try {
        const done = await send(run, step.label, id, cmd);
        if (!done.sent) throw new Error("its result came without a `sent` before it");
        answer = { seq: done.result.seq, nonce: done.sent.nonce, latency_us: done.result.latency_us, rtt_ms: done.rtt_ms };
      } catch (e) {
        if (e instanceof Stopped) throw e;
        if (e.kind !== "unknown_outcome") throw new Error(`${what}: ${e.message}`);
        answer = await recover(run, step, what, id, cmd, e);
        recovered = true;
      }
      const seq = answer.seq;
      const nonce = answer.nonce;
      // An ordinary command's nonce is the gateway's: it is what "reuse" and "skip" count from.
      if (cmd.nonce === undefined) run.lastNonce.set(cmd.actor, nonce);
      const merged = seq == null ? null : recorder.batches.get(seq);
      if (seq != null) {
        ctx.recordLabel(step.label, seq);
        if (merged) ctx.observe(merged.events);
      }
      const rejection = merged ? rejectionOf(merged.events) : null;
      const record = {
        step: step.label,
        at: step.at,
        actor: cmd.actor,
        action: actionName(cmd.action),
        nonce,
        seq,
        late_ms,
        latency_us: answer.latency_us,
        rtt_ms: answer.rtt_ms,
        rejected: rejection,
        recovered,
      };
      result.commands.push(record);
      const timing = recovered ? "recovered" : `${answer.latency_us} us, rtt ${answer.rtt_ms.toFixed(1)} ms`;
      const late = late_ms > 50 ? `, ${late_ms} ms late` : "";
      run.log(`+${step.at} ${what} nonce ${nonce} -> seq ${seq ?? "unknown"} (${timing}${late})${rejection ? ` rejected: ${rejection}` : ""}`);
      check(run);
    }

    /**
     * The engine stopped answering after the command was sent: it may or may not have been applied
     * (gateway.md §6). Wait until the engine is up again, then resend the same action with the same
     * nonce: if the first was applied the engine rejects the repeat as Duplicate, so the command
     * happens exactly once either way. Returns what `runStep` needs: the seq of the batch that applied
     * the command (the original's when the retry was a Duplicate), the nonce and the timings.
     */
    async function recover(run, step, what, id, cmd, first) {
      let err = first;
      for (let attempt = 1; attempt <= MAX_RETRIES; attempt++) {
        const nonce = err.nonce;
        run.log(`${what}: outcome unknown (the engine stopped answering after nonce ${nonce} was sent); waiting for the engine`);
        await orStop(run, gateway.untilEngineUp({ after: err.frameNo, timeoutMs: engineWait }));
        run.artifacts.add(`${cmd.actor}:${nonce}`);
        let answer;
        try {
          answer = await send(run, step.label, `${id}.retry${attempt}`, { ...cmd, nonce });
        } catch (e) {
          if (e instanceof Stopped) throw e;
          if (e.kind === "unknown_outcome" && attempt < MAX_RETRIES) {
            err = e;
            continue;
          }
          throw new Error(`${what}: retry with nonce ${nonce} failed: ${e.message}`);
        }
        const retried = recorder.batches.get(answer.result.seq);
        let seq = answer.result.seq;
        let how;
        if (cmd.nonce !== undefined) {
          // An attack step sends a nonce that is not its own to consume (a replay or a gap): the engine
          // rejects it however often it is sent, so nothing can have been applied and the retry is
          // another such rejection. (Looking for "the batch that applied it" would find the batch of
          // the command that really owns the nonce.)
          how = "rejected by design either way (the step's nonce is not its own), so the retry changed nothing";
        } else if (retried && rejectionOf(retried.events) === "Duplicate") {
          // Applied before the outage: the original's batch is the one with the command's origin that
          // consumed the nonce, which the recorder has if it was heard (the admin view hears all).
          const original = recorder
            .batchList()
            .find((b) => b.origin?.actor === cmd.actor && b.origin.nonce === nonce && b.seq !== seq && !unconsumed(b.events));
          seq = original ? original.seq : null;
          how = original
            ? "applied once (Duplicate on retry)"
            : "applied once (Duplicate on retry), but its own batch was never heard here: its seq, and so what it made, is unknown";
        } else {
          // Not applied before, or the original's late batch answered the resend (gateway.md §6): the
          // command's batch is the answer's either way.
          how = "applied on retry";
        }
        run.log(`${what}: outcome recovered: ${how}`);
        run.result.recoveries.push({ step: step.label, nonce, outcome: how, seq, retry_seq: answer.result.seq });
        return { seq, nonce, latency_us: answer.result.latency_us, rtt_ms: answer.rtt_ms };
      }
      throw new Error(`${what}: the outcome stayed unknown after ${MAX_RETRIES} retries`);
    }

    /**
     * Runs each of `names` `n` times, `k` at a time, and returns every run's verdict and the aggregate
     * perf: `{ runs, passed, failed, pass, stopped, perf }`. Runs are scoped (accounts `t0 * 100 + id`,
     * contract `t0`, distinct `t0` 1 ms apart at least), so none sees another's events. `runs` is in the
     * order the jobs were given (names interleaved: each name once, then again, ...); after a Stop it
     * holds only the runs that had started.
     */
    async function runMany(names, n, k) {
      if (!Number.isInteger(n) || n < 1) throw new RangeError(`n must be a whole number of runs, got ${n}`);
      if (!Number.isInteger(k) || k < 1) throw new RangeError(`k must be a whole number of runs at a time, got ${k}`);
      if (names.length === 0) throw new RangeError("no scenarios to run");
      for (const name of names) if (typeof name === "string" && !byName(name)) throw new Error(`no scenario named ${name}`);
      const gen = generation;
      await prepare();
      const jobs = [];
      for (let i = 0; i < n; i++) jobs.push(...names);
      const runs = new Array(jobs.length);
      let next = 0;
      const started = Date.now();
      const worker = async () => {
        while (next < jobs.length && gen === generation) {
          const i = next++;
          runs[i] = await runOne(jobs[i], gen);
        }
      };
      await Promise.all(Array.from({ length: Math.min(k, jobs.length) }, worker));
      const wall_ms = Date.now() - started;
      const done = runs.filter(Boolean);
      const passed = done.filter((r) => r.pass).length;
      return {
        runs: done,
        passed,
        failed: done.length - passed,
        pass: passed === jobs.length,
        stopped: gen !== generation,
        concurrency: k,
        perf: R.aggregatePerf(
          done.map((r) => ({ commands: r.commands.filter((c) => !c.recovered) })),
          wall_ms,
        ),
      };
    }

    /** One run of `scenario` (a name, or a scenario object like the ones in `scenarios`): its verdict, as in `runMany(...).runs`. */
    async function run(scenario) {
      const gen = generation;
      await prepare();
      return runOne(scenario, gen);
    }

    /** Abandons the runs in progress and the run sets' jobs not started yet. The driver stays usable. */
    function stop() {
      generation++;
      stopSignal.fire();
      stopSignal = signal();
    }

    /** Stops, and lets go of the connection: no more listening, recording or running. */
    function dispose() {
      if (disposed) return;
      disposed = true;
      stop();
      for (const off of offs) off();
      DRIVEN.delete(gateway);
    }

    return {
      recorder,
      scenarios,
      run,
      runMany,
      stop,
      dispose,
      engineNow,
      commandAt: (seq) => bySeq.get(seq),
      get active() {
        return [...active].map((r) => r.result);
      },
      get disposed() {
        return disposed;
      },
    };
  }

  return {
    scenarios,
    byName,
    names: () => scenarios.map((s) => s.name),
    Ctx,
    remap,
    resolveStep,
    createDriver,
    START_MARGIN_MS,
    WAIT_GRACE_MS,
    GRACE_MS,
  };
})();
if (typeof module !== "undefined") module.exports = RfqScenarios;
