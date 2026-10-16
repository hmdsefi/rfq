// The 14 scenarios as data (eight brief ones, six attacks), and the driver that plays one as its
// participants through the gateway: dashboard -> WebSocket -> rfq-gateway -> Aeron -> rfq-node. Plain
// script (inlined in the page, or `require`d by Node); it needs recorder.js (and, in the page, that
// script comes first).
//
//   const driver = RfqScenarios.createDriver(gateway);              // gateway: RfqGateway.connect(...)
//   const { runs, perf } = await driver.runMany(["happy_path"], 20, 4);
//   const earlier = RfqScenarios.runsFromHistory(entries);          // the runs in a read API admin trace
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
  /**
   * How late a run's step may be sent and still count as on time, for a history: one that ends this long after
   * a script's next step was due, without it, is not that script's run (the step would have been sent).
   */
  const LATE_MS = 250;

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

    /**
     * The run's `k`th extra market (`k` in 1..=9), for a multi-leg script: `t0 * 10 + k` (testkit's
     * `Ctx::contract_n`). Distinct runs never collide, and for a millisecond clock the id stays below
     * 2^53, so it is exact in a JS number. The run's scope owns these ids too (`Scope.ownsContract`).
     */
    contractN(k) {
      if (!Number.isInteger(k) || k < 1 || k > 9) throw new RangeError(`a run has extra markets 1..=9, not ${k}`);
      const id = this.t0 * 10 + k;
      if (!Number.isSafeInteger(id)) throw new RangeError(`market id ${id} is not a safe integer`);
      return id;
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

  /** Deposits $1,000 for Alice, both market makers and Carol. Rust: `rfq_testkit::scenarios::fund`. */
  const fund = () =>
    [
      ["fund_alice", ALICE],
      ["fund_mm1", MM1],
      ["fund_mm2", MM2],
      ["fund_carol", CAROL],
    ].map(([label, to]) => send(0, label, OPERATOR, () => ({ Deposit: { to, amount: dollars(1_000) } })));

  /** The operator lists a market that closes at +5 s, with a hard deadline at +15 s. Rust: `scenarios::list`. */
  const list = (label, id, description) =>
    send(0, label, OPERATOR, (c) => ({
      ListContract: {
        id: id(c),
        description,
        oracle: ORACLE,
        trading_close: c.at(5_000),
        resolution_deadline: c.at(15_000),
        dispute_window: 2_000,
      },
    }));

  /**
   * Funds everyone and lists the run's contract (closes at +5 s, hard deadline at +15 s).
   * Rust: `rfq_testkit::scenarios::setup`; the dashboard's `presets::setup` is the same.
   */
  const setup = () => [...fund(), list("list", (c) => c.contract(), "Will it rain in Paris on 2026-11-01?")];

  /** Funds everyone and lists three markets (Paris, London, Rome), each closing at +5 s. Rust: `scenarios::three_markets`. */
  const threeMarkets = () => [
    ...fund(),
    list("list_paris", (c) => c.contractN(1), "Will it rain in Paris on 2026-11-01?"),
    list("list_london", (c) => c.contractN(2), "Will it rain in London on 2026-11-01?"),
    list("list_rome", (c) => c.contractN(3), "Will it rain in Rome on 2026-11-01?"),
  ];

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

  /** One request for 400 YES on each of the three markets (limit 0.70 each), quotes due at +1.5 s. Rust: `scenarios::three_leg_request`. */
  const threeLegRequest = () =>
    send(100, "req", ALICE, (c) => ({
      SubmitRequest: {
        legs: [1, 2, 3].map((k) => ({ contract: c.contractN(k), side: "BuyYes", qty: 400, limit_price: price(7_000) })),
        response_deadline: c.at(1_500),
      },
    }));

  /** `mm` quotes leg `leg` of the request at `bps`, firm to +10 s. Rust: `scenarios::leg_quote`. */
  const legQuote = (at, label, mm, leg, bps) =>
    send(at, label, mm, (c) => ({
      SubmitQuote: { request: c.request("req"), leg, price: price(bps), size: 400, expires_at: c.at(10_000) },
    }));

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
    multi_leg_happy:
      "Alice asks for 400 YES in each of three markets in one request; MM1 quotes legs 1 and 3, MM2 leg 2. One accept fills all three legs.",
    multi_leg_leg2_fails:
      "Leg 2 of 3 is never quoted: at the deadline the whole request expires, no leg fills, and every hold is released in that one command.",
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
    // Rust: `rfq_testkit::scenarios::all()`, `multi_leg_happy` (engine.md §8). Three markets, one request, a
    // quote on every leg, and one accept fills all three legs. MM1 backs leg indices 0 and 2, MM2 index 1.
    // `markets`: how many extra markets the run lists (`ctx.contractN(1..=markets)`); a script without it
    // lists the run's one contract, `t0`.
    {
      name: "multi_leg_happy",
      kind: "brief",
      markets: 3,
      steps: [
        ...threeMarkets(),
        threeLegRequest(),
        legQuote(300, "q1", MM1, 0, 6_200),
        legQuote(400, "q2", MM2, 1, 6_000),
        legQuote(500, "q3", MM1, 2, 6_400),
        wait(1_600),
        accept(2_000),
      ],
      expect: [
        "RequestOpened",
        "RequestOpened",
        "RequestOpened",
        "QuoteSubmitted",
        "QuoteSubmitted",
        "QuoteSubmitted",
        "QuoteSelected",
        "QuoteSelected",
        "QuoteSelected",
        "RequestPresented",
        "QuoteFilled",
        "QuoteFilled",
        "QuoteFilled",
        "RequestFilled",
      ],
    },
    // Rust: `rfq_testkit::scenarios::all()`, `multi_leg_leg2_fails`. Leg 2 of 3 fails: legs 1 and 3 are
    // quoted, leg 2 is not, so at the deadline the whole request expires and the requester's and both MMs'
    // holds are all released in that one command.
    {
      name: "multi_leg_leg2_fails",
      kind: "brief",
      markets: 3,
      steps: [
        ...threeMarkets(),
        threeLegRequest(),
        legQuote(300, "q1", MM1, 0, 6_200),
        legQuote(400, "q3", MM2, 2, 6_000),
        wait(1_600),
      ],
      expect: [
        "RequestOpened",
        "RequestOpened",
        "RequestOpened",
        "QuoteSubmitted",
        "QuoteSubmitted",
        "QuoteLost",
        "QuoteLost",
        "RequestExpired",
      ],
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

  /**
   * One batch into a run's record (`result`), the same for a live run and one rebuilt from the history: the
   * run's own events go into `own`, and the batch's seq into `batches` when it is one of the run's commands'
   * or holds an event of the run. A retry's Duplicate (`artifact`) keeps the scope's bookkeeping and owns
   * nothing, so it cannot satisfy an expect.
   */
  function takeBatch(scope, result, frame, artifact) {
    let mine = frame.origin != null && scope.origins.has(`${frame.origin.actor}:${frame.origin.nonce}`);
    for (const e of frame.events) {
      if (scope.owns(frame, e) && !artifact) {
        mine = true;
        result.own.push({ seq: frame.seq, time: frame.time, kind: frame.kind, audience: e.audience, event: e.event });
      }
    }
    if (mine && result.batches.at(-1) !== frame.seq) result.batches.push(frame.seq);
  }

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
   * of the connection; `commandAt(seq, epoch = recorder.epoch)` is the command the driver sent that
   * made batch `seq` of that journal epoch (seqs start again after a journal reset).
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
    /** "epoch:seq" -> the command whose `result` named that batch (seqs start again after a journal reset). */
    const bySeq = new Map();
    let lastT0 = 0;
    let runNumber = 0;
    let disposed = false;
    let measuring = null; // the clock measurement in progress
    // The frame number of an answer that shows the engine down (`engine unavailable` or `outcome
    // unknown`) and no `up` since. The gateway sends those answers before its `status down`, and in a
    // browser the failed run ends between the two frames: a run set must not start the next run on
    // the stale "up".
    let downSince = null;
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
      gateway.on("error", (frame, meta) => {
        if (frame.error === "engine unavailable" || frame.error?.startsWith("outcome unknown:")) downSince ??= meta.n;
      }),
      gateway.on("status", (frame) => {
        if (frame.engine === "up") downSince = null;
      }),
      gateway.on("welcome", (frame) => {
        if (frame.engine === "up") downSince = null;
      }),
      // A result (a late one too, after an unknown outcome) names the batch its command made.
      gateway.on("result", (frame) => {
        const cmd = sentById.get(frame.id);
        // The recorder has the result's batch already (it comes first), so its epoch is the batch's.
        if (cmd) bySeq.set(`${recorder.epoch}:${frame.seq}`, cmd);
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

    /** The current generation and its stop signal: a run or run set started now is stopped by the next `stop()`. */
    const current = () => ({ gen: generation, stopped: stopSignal.promise });

    /** `promise`, or `Stopped` as soon as generation `at` is stopped. */
    function orStopped(at, promise) {
      if (at.gen !== generation) return Promise.reject(new Stopped());
      return Promise.race([promise, at.stopped.then(() => Promise.reject(new Stopped()))]);
    }

    /**
     * Before runs: the admin view, the engine up, and the clocks measured against each other afresh
     * (they drift). Only while no run is active: a run in progress schedules its steps by the offset,
     * so it must not move under it. A Stop ends the wait for the engine at once.
     */
    async function prepare(at) {
      if (disposed) throw new Error("this driver is disposed");
      if (gateway.view !== "admin") {
        throw new Error("scenarios need the admin view: the verdict reads every event, sealed ones too");
      }
      if (gateway.engine !== "up") await orStopped(at, gateway.untilEngineUp({ timeoutMs: engineWait }));
      if (measuring) await measuring;
      else if (active.size === 0) {
        measuring = gateway.syncClock(3).finally(() => (measuring = null));
        await measuring;
      }
    }

    /**
     * Plays one run. `before` is what happened before it started, in a run set: `waitedMs` for the
     * engine to be up, or `blocked`, the error that keeps it from starting (it then fails, sending
     * nothing).
     */
    async function runOne(which, gen, before = {}) {
      const scenario = typeof which === "string" ? byName(which) : which;
      if (!scenario) throw new Error(`no scenario named ${which}; the scenarios are ${scenarios.map((s) => s.name).join(", ")}`);
      const run = newRun(scenario, allocT0(), gen);
      active.add(run);
      opts.onRunStart?.(run.result);
      try {
        if (before.blocked) throw before.blocked;
        if (before.waitedMs != null) run.log(`the engine was down: waited ${before.waitedMs} ms for it before starting`);
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
          const artifact = origin != null && run.artifacts.has(origin) && unconsumed(frame.events);
          if (artifact) run.artifacts.delete(origin);
          takeBatch(scope, result, frame, artifact);
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
      const lists = scenario.markets ? `markets ${ctx.contractN(1)}..${ctx.contractN(scenario.markets)}` : `contract ${t0}`;
      run.log(`${scenario.name}: t0=${t0} (engine now + ${startMargin} ms), ${lists}`);
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
        // Never "applied" for a rejection: a rejected command consumed its nonce too, and a Duplicate
        // on the retry proves only that the nonce was taken, not that the original succeeded.
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
          const taken = `nonce ${nonce} taken once (Duplicate on retry)`;
          const rejected = original ? rejectionOf(original.events) : null;
          how = !original
            ? `${taken}; original not heard — whether it was applied is unknown`
            : rejected
              ? `${taken}; original: rejected: ${rejected}`
              : `${taken}; original: applied`;
        } else {
          // Not taken before, or the original's late batch answered the resend (gateway.md §6): the
          // command's batch is the answer's either way, and it says whether it was applied.
          const rejected = retried ? rejectionOf(retried.events) : null;
          how = !retried
            ? "retry answered, but its batch was never heard here — whether it was applied is unknown"
            : rejected
              ? `retry rejected: ${rejected}`
              : "retry applied";
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
     * contract `t0` or markets `t0 * 10 + k`, distinct `t0` 1 ms apart at least), so none sees another's
     * events. `runs` is in the order the jobs were given (names interleaved: each name once, then again, ...); after a Stop it
     * holds only the runs that had started.
     */
    async function runMany(names, n, k) {
      if (!Number.isInteger(n) || n < 1) throw new RangeError(`n must be a whole number of runs, got ${n}`);
      if (!Number.isInteger(k) || k < 1) throw new RangeError(`k must be a whole number of runs at a time, got ${k}`);
      if (names.length === 0) throw new RangeError("no scenarios to run");
      for (const name of names) if (typeof name === "string" && !byName(name)) throw new Error(`no scenario named ${name}`);
      const at = current();
      const gen = at.gen;
      try {
        await prepare(at);
      } catch (e) {
        if (!(e instanceof Stopped)) throw e;
        // Stopped before any run started (it was waiting for the engine).
        return { runs: [], passed: 0, failed: 0, pass: false, stopped: true, concurrency: k, perf: R.aggregatePerf([], 0) };
      }
      const jobs = [];
      for (let i = 0; i < n; i++) jobs.push(...names);
      const runs = new Array(jobs.length);
      let next = 0;
      const started = Date.now();
      // The engine went down during the set: each next run waits for it (up to `engineWait`, a Stop
      // ends the wait) instead of failing at once with `engine unavailable`. Once a wait has timed
      // out, the runs after it fail at once while the engine stays down, so the set still ends.
      let gaveUp = null;
      const engineBack = async () => {
        if (gateway.engine === "up" && downSince == null) {
          gaveUp = null;
          return {};
        }
        if (gaveUp) return { blocked: gaveUp };
        const since = Date.now();
        try {
          await orStopped(at, gateway.untilEngineUp({ after: downSince ?? undefined, timeoutMs: engineWait }));
        } catch (e) {
          if (e instanceof Stopped) throw e;
          gaveUp ??= new Error(`not started: ${e.message}`);
          return { blocked: gaveUp };
        }
        return { waitedMs: Date.now() - since };
      };
      const worker = async () => {
        while (next < jobs.length && gen === generation) {
          const i = next++;
          let before;
          try {
            before = await engineBack();
          } catch (e) {
            if (!(e instanceof Stopped)) throw e;
            break; // stopped while it waited: this run never started
          }
          runs[i] = await runOne(jobs[i], gen, before);
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

    /**
     * One run of `scenario` (a name, or a scenario object like the ones in `scenarios`): its verdict, as
     * in `runMany(...).runs`. Rejects with `Stopped` if a Stop came while it waited for the engine.
     */
    async function run(scenario) {
      const at = current();
      await prepare(at);
      return runOne(scenario, at.gen);
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
      /** The command that made batch `seq` of `epoch` (default: the recorder's current epoch). */
      commandAt: (seq, epoch = recorder.epoch) => bySeq.get(`${epoch}:${seq}`),
      get active() {
        return [...active].map((r) => r.result);
      },
      get disposed() {
        return disposed;
      },
    };
  }

  // ---- history: the runs a reloaded page finds in the read API's trace ----------------------------------

  /** The scripts' own accounts (all but the genesis roles), which a run's scope maps to `t0 * 100 + id`. */
  const RUN_ACCOUNTS = Object.keys(ACCOUNT_NAMES).map(Number).filter((a) => !R.GENESIS_ROLES.includes(a));
  const PER_RUN = R.ACCOUNTS_PER_RUN;

  /** `v` as JSON with every object's keys sorted: two actions compare equal whatever order their fields came in. */
  const canonical = (v) =>
    JSON.stringify(v, (_, x) => (x && typeof x === "object" && !Array.isArray(x) ? Object.fromEntries(Object.keys(x).sort().map((k) => [k, x[k]])) : x));

  /** A wall-clock time as the page shows one, `14:03:12`. */
  const clockOf = (ms) => new Date(ms).toTimeString().slice(0, 8);

  /** The accounts and contracts an action names besides its actor (a request or quote is named by a seq). */
  function named(action) {
    const [name, b] = Object.entries(action ?? {})[0] ?? [];
    switch (name) {
      case "Deposit":
        return { accounts: [b.to], contracts: [] };
      case "ListContract":
        return { accounts: [b.oracle], contracts: [b.id] };
      case "SubmitRequest":
        return { accounts: [], contracts: (b.legs ?? []).map((l) => l.contract) };
      case "ProposeOutcome":
      case "Dispute":
      case "Rule":
        return { accounts: [], contracts: [b.contract] };
      default:
        return { accounts: [], contracts: [] };
    }
  }

  /**
   * The runs (`t0`) a history shows: every account its batches name (an actor, an account a command names,
   * a balance) that is a script's account of a run, `t0 * 100 + id`. Accounts below 100, like the README's
   * curl walk-through's, belong to no run.
   */
  function runT0s(entries) {
    const t0s = new Set();
    const note = (a) => {
      if (Number.isSafeInteger(a) && a >= PER_RUN && RUN_ACCOUNTS.includes(a % PER_RUN)) t0s.add(Math.floor(a / PER_RUN));
    };
    for (const e of entries) {
      note(e.origin?.actor);
      if (e.command) [e.command.actor, ...named(e.command.action).accounts].forEach(note);
      for (const b of e.balances ?? []) note(b.account);
    }
    return t0s;
  }

  /** Whether `cmd` is the run's: its actor is the run's, or a shared genesis role acts on the run's accounts or contracts. */
  function isRunCommand(scope, cmd) {
    if (scope.accounts.has(cmd.actor)) return true;
    if (!R.GENESIS_ROLES.includes(cmd.actor)) return false;
    const { accounts, contracts } = named(cmd.action);
    return accounts.some((a) => scope.accounts.has(a)) || contracts.some((c) => scope.ownsContract(c));
  }

  /** A script's commands as `"<actor> <action>"`, the actor a script account or genesis role, in order. */
  function shapeOf(scenario) {
    const ctx = new Ctx(0);
    return scenario.steps
      .filter((s) => !s.wait)
      .map((step, i) => {
        const shape = `${step.actor} ${actionName(step.action(ctx))}`;
        ctx.recordLabel(step.label, i + 1); // a later step may name this one's request or quote
        return shape;
      });
  }

  /**
   * Whether `scenario`'s script, played as the driver plays it against what the history says the engine
   * answered (its seqs, the picks it presented), starts with exactly the run's commands so far (`steps`).
   * `from` is the index of the run's first batch: nothing before it is about the run.
   */
  function replays(scenario, t0, steps, batches, from = 0) {
    const scope = new R.Scope(t0);
    const ctx = new Ctx(t0, (a) => scope.account(a));
    const lastNonce = new Map();
    let heard = from;
    const script = scenario.steps.filter((s) => !s.wait);
    for (const [i, { cmd, seq }] of steps.entries()) {
      const step = script[i];
      // What the driver had heard when it sent this step: every batch before it.
      for (; heard < batches.length && batches[heard].seq < seq; heard++) ctx.observe(batches[heard].events);
      let want;
      try {
        want = resolveStep(step, ctx, (actor) => lastNonce.get(actor));
      } catch {
        return false;
      }
      if (want.actor !== cmd.actor || canonical(want.action) !== canonical(cmd.action)) return false;
      if (want.nonce !== undefined && want.nonce !== cmd.nonce) return false;
      if (want.nonce === undefined) lastNonce.set(cmd.actor, cmd.nonce);
      ctx.recordLabel(step.label, seq);
    }
    return true;
  }

  /**
   * The scripts the run's commands so far (`steps`, retries left out) may come from: the ones that start with
   * the same actors (as script accounts or genesis roles) and action names in the same order, and whose script,
   * replayed, sends exactly these actions. The shape alone says nothing of legs, prices or picks: a
   * `multi_leg_happy` stopped after its second quote has `multi_leg_leg2_fails`'s shape, and the wrong-price and
   * double accepts share one.
   */
  function candidatesOf(t0, steps, batches, from) {
    const role = (a) => (R.GENESIS_ROLES.includes(a) ? a : a - t0 * PER_RUN);
    const shape = steps.map(({ cmd }) => `${role(cmd.actor)} ${actionName(cmd.action)}`);
    return scenarios.filter((s) => {
      const script = shapeOf(s);
      return script.length >= shape.length && shape.every((x, i) => x === script[i]) && replays(s, t0, steps, batches, from);
    });
  }

  /** The time the `n`th command step (from 0) of `scenario` is due in run `t0`, or null when it has fewer. */
  const dueOf = (scenario, t0, n) => {
    const step = scenario.steps.filter((s) => !s.wait)[n];
    return step ? t0 + step.at : null;
  };

  /**
   * Where the driver stops listening to a run of `scenario` (see `play`): at its last step (a wait's end is
   * `WAIT_GRACE_MS` after its time; a command's, its batch, a retry's if it had one), then on while its expected
   * events have not all come, `GRACE_MS` at most (`until`). `takes(b, own)`: whether the run takes batch `b`,
   * given the events it owns so far; batches come in seq order, so the first it does not take ends the run.
   * `closed(own, end)`: whether the window has ended by engine time `end`, every batch up to it taken.
   */
  function windowOf(scenario, t0, cmds) {
    const last = scenario.steps.at(-1);
    const lastCmd = cmds.at(-1);
    const endTime = last.wait ? t0 + last.at + WAIT_GRACE_MS : lastCmd.time;
    const met = (own) => R.containsInOrder(own, scenario.expect).ok;
    return {
      until: endTime + GRACE_MS,
      takes: (b, own) => (last.wait ? b.time <= endTime : b.seq <= lastCmd.seq) || (b.time <= endTime + GRACE_MS && !met(own)),
      closed: (own, end) => (!last.wait || end > endTime) && (end > endTime + GRACE_MS || met(own)),
    };
  }

  /**
   * Run `t0` of a history (`batches` ascending by seq), rebuilt as the driver records a live run, as of engine
   * time `end` (every batch up to it is in `batches`; at least the last batch's time). `cutAt` is where the
   * history fetched from the read API ended, `end` unless this is a later rebuild with the batches recorded
   * since (`settleRun`). See `runsFromHistory` for what comes back.
   */
  function historyRun(t0, batches, { end = -Infinity, cutAt } = {}) {
    end = Math.max(end, batches.at(-1)?.time ?? -Infinity);
    cutAt ??= end;
    const scope = new R.Scope(t0);
    const accounts = {};
    for (const a of RUN_ACCOUNTS) accounts[ACCOUNT_NAMES[a]] = scope.account(a);
    // The run's commands, in seq order, each sent as the driver sends one. A retry repeats an earlier
    // command exactly (actor, nonce and action) and is refused as a Duplicate (or a NonceGap): a recovery
    // artifact, like the driver's. An attack's replay sends another action with a used nonce: the run's own.
    const cmds = [];
    const before = new Set();
    for (const b of batches) {
      const cmd = b.command;
      if (!cmd || !isRunCommand(scope, cmd)) continue;
      const key = canonical([cmd.actor, cmd.nonce, cmd.action]);
      cmds.push({ cmd, seq: b.seq, time: b.time, events: b.events, retry: before.has(key) && unconsumed(b.events) });
      before.add(key);
      scope.sent(cmd.actor, cmd.nonce);
      if (R.GENESIS_ROLES.includes(cmd.actor)) accounts[ACCOUNT_NAMES[cmd.actor]] = cmd.actor;
    }
    const steps = cmds.filter((c) => !c.retry);
    const retries = new Set(cmds.filter((c) => c.retry).map((c) => c.seq));
    // Nothing before the run's first command is about its accounts or contracts.
    const from = cmds.length ? batches.findIndex((b) => b.seq === cmds[0].seq) : 0;
    const candidates = cmds.length ? candidatesOf(t0, steps, batches, from) : [];
    const whole = candidates.filter((s) => shapeOf(s).length === steps.length);
    // A script with a step still to send when the history was cut (due less than LATE_MS before its end):
    // the run may have been sending it, and the page that sent its commands reloaded.
    const going = candidates.filter((s) => {
      const due = dueOf(s, t0, steps.length);
      return due != null && due > cutAt - LATE_MS;
    });
    // A command of one of the run's accounts with no `command`: a live batch, which carries no action.
    const unread = batches.some((b) => !b.command && b.origin != null && scope.accounts.has(b.origin.actor));
    const cut = going.length > 0 || unread;
    // Judged only when the history holds the whole of one script's commands, and nothing could follow them.
    const scenario = !cut && whole.length === 1 ? whole[0] : null;
    // Named when only one script fits, judged or not.
    const possible = [...whole, ...going.filter((s) => !whole.includes(s))];
    const named = scenario ?? (cut && possible.length === 1 ? possible[0] : null);
    const result = { own: [], batches: [] };
    const span = scenario ? windowOf(scenario, t0, cmds) : null;
    let closed = !span;
    let lastTime = t0;
    for (let i = from; i < batches.length; i++) {
      const b = batches[i];
      if (span && !span.takes(b, result.own)) {
        closed = true;
        break;
      }
      takeBatch(scope, result, b, retries.has(b.seq));
      if (result.batches.at(-1) === b.seq) lastTime = b.time;
    }
    closed ||= span.closed(result.own, end);
    // Its window goes on past the history: the batches recorded after it finish the run (`settleRun`).
    const open = !closed;
    const script = named ? named.steps.filter((s) => !s.wait) : [];
    const commands = steps.map((c, i) => ({
      step: script[i]?.label ?? null,
      at: script[i]?.at ?? null,
      actor: c.cmd.actor,
      action: actionName(c.cmd.action),
      nonce: c.cmd.nonce,
      seq: c.seq,
      late_ms: null,
      latency_us: null,
      rtt_ms: null,
      rejected: rejectionOf(c.events),
      recovered: cmds.some((r) => r.retry && r.cmd.actor === c.cmd.actor && r.cmd.nonce === c.cmd.nonce),
    }));
    const unclassified = [...scope.unclassified];
    const judged = scenario && closed;
    const verdict = judged ? R.containsInOrder(result.own, scenario.expect) : null;
    const incomplete = cut || open;
    const detail = unread
      ? "commands of the run came after the history, and the live stream does not carry their actions: it has no verdict"
      : cut
        ? "the history ends while the run was still sending its commands (the page that ran it reloaded): it has no verdict"
        : open
          ? "the history ends before the run did: its verdict comes once the engine has finished it"
          : !judged
            ? null
            : unclassified.length
              ? `the engine emitted ${unclassified.join(", ")}, which the scope has no rule for (update recorder.js)`
              : verdict.error;
    const pass = judged ? !unclassified.length && verdict.ok : null;
    const name = named?.name ?? `run ${clockOf(t0)}`;
    const seqs = result.batches;
    return {
      id: `h${t0}`,
      history: true,
      name,
      kind: named?.kind ?? null,
      t0,
      contract: t0,
      accounts,
      expect: named?.expect ?? [],
      pass,
      // No verdict yet or ever: cut while it sent its commands, or still going (`open`) at the history's end.
      incomplete,
      open,
      cutAt,
      until: open ? span.until : null,
      stopped: false,
      detail,
      error: null,
      step: named ? named.steps.length : steps.length,
      steps: named ? named.steps.length : steps.length,
      own: result.own,
      batches: seqs,
      labels: Object.fromEntries(commands.filter((c) => c.step).map((c) => [c.step, c.seq])),
      commands,
      recoveries: [],
      log: [
        `${name}: t0=${t0}, rebuilt from the history: ${commands.length} command${commands.length === 1 ? "" : "s"}, batches ${seqs[0]}..${seqs.at(-1)}`,
        incomplete ? `incomplete: ${detail}` : !judged ? "no verdict: its commands are not one scenario's script" : pass ? "PASS" : `FAIL: ${detail}`,
      ],
      // Engine time: the run's first step is due at t0, and its last batch ends it.
      startedAt: t0,
      wall_ms: Math.max(0, lastTime - t0),
      perf: null,
      done: true,
    };
  }

  /**
   * The scenario runs in a history (`entries`: the read API's admin trace, ascending by seq, each with its
   * `origin` and `command`), one per `t0` that `runT0s` finds, by `t0`. `end` is the engine time the history
   * is known to be whole up to: by default its last entry's.
   *
   * Each is rebuilt as the driver records a live run, so the page's run machinery takes it as it is: a scope
   * fed the run's accounts, its commands (`sent`) and then every batch in seq order gives its `own` events and
   * `batches`, up to where the driver would have stopped listening. Its name is the scenario whose script
   * matches its commands (`candidatesOf`), else the neutral `run <time>`; its verdict is that script's expect
   * list on `own` (`pass` true or false). Marked `history: true`, with the id `h<t0>`, and never running.
   *
   * Never a verdict too early (`pass: null`, `incomplete: true`): a run whose commands so far start a script
   * that still had a step to send when the history ends was cut by a reload, and keeps no verdict; a run that
   * had sent all of one script's commands but whose window goes on past the end is `open` until `until`, and
   * `settleRun` finishes it from the batches recorded since. A run with no batch is left out.
   */
  function runsFromHistory(entries, { end } = {}) {
    const batches = [...entries].sort((a, b) => a.seq - b.seq);
    return [...runT0s(batches)]
      .sort((a, b) => a - b)
      .map((t0) => historyRun(t0, batches, { end }))
      .filter((r) => r.batches.length > 0);
  }

  /**
   * An `open` history run again, as of engine time `end`, from `batches`: the history it came from and every
   * batch recorded since (any order; the live ones carry no `command`). It is judged once its window has
   * ended by then; a command of one of its accounts that came after the history leaves it incomplete for good.
   */
  function settleRun(run, batches, end) {
    return historyRun(run.t0, [...batches].sort((a, b) => a.seq - b.seq), { end, cutAt: run.cutAt });
  }

  return {
    scenarios,
    byName,
    names: () => scenarios.map((s) => s.name),
    runsFromHistory,
    settleRun,
    runT0s,
    Ctx,
    remap,
    resolveStep,
    createDriver,
    Stopped,
    START_MARGIN_MS,
    WAIT_GRACE_MS,
    GRACE_MS,
  };
})();
if (typeof module !== "undefined") module.exports = RfqScenarios;
