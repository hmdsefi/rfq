// The model: what the panels show at one position of a recording. Plain script (inlined in the page,
// or `require`d by Node); no dependencies.
//
//   const m = RfqModel.modelAt({ batches, commandAt, viewer, accountInfo }, position);
//
// `batches` are the merged batches of one connection, in arrival order (a recorder's `allBatches()`,
// or a run's part of them). `position` is an index into them: the model is built from batches 0..position.
// Object states come from the events (each event is a transition). Balances come from `balances`
// only. What the events don't carry (a requester's limit, a quote's size, a contract's terms) comes
// from the command the page itself sent, `commandAt(seq)`, joined only through a batch's `origin`: the
// gateway forwards an origin only to the view that sent the command (gateway.md §4), so in an account
// view the page's knowledge of the other participants' commands never reaches the model.
//
// Nothing here computes money or fires a timer: every amount is a `balances` value or an event field.
const RfqModel = (() => {
  const variant = (v) => (typeof v === "string" ? v : Object.keys(v)[0]);
  const body = (v) => (typeof v === "string" ? {} : Object.values(v)[0]);
  const KIND = { User: "user", Tick: "tick", Resume: "resume", Genesis: "genesis" };

  /**
   * The page's command that made `batch`, or null: only through the batch's origin, and only if the
   * command is that origin's (same actor and nonce).
   */
  function commandOf(recording, batch) {
    const o = batch.origin;
    if (!o || typeof recording.commandAt !== "function") return null;
    const c = recording.commandAt(batch.seq);
    return c && c.actor === o.actor && c.nonce === o.nonce ? c : null;
  }

  /** The action of `cmd` if it is a `name` action, else null. */
  const actionOf = (cmd, name) => (cmd && cmd.action && variant(cmd.action) === name ? cmd.action[name] : null);

  /**
   * The model at `position` (default: the last batch; -1: before the first). Returns
   * `{ position, count, seq, epoch, now, viewer, accounts, previous, requests, quotes, contracts,
   * positions, trace }`, in the shape the page's renderers read:
   *   - `accounts`: `[{ id, name, role, balance }]`, each with its last `balances` value up to the position;
   *     `previous` is the same as a Map (id -> balance) one position earlier, for the ledger's deltas;
   *   - `requests`, `quotes`, `contracts`, `positions`: what this connection's events say about them;
   *     a field nothing said is null (sealed, or not in this recording);
   *   - `trace`: one entry per batch, `{ key, seq, epoch, time, kind, actor, nonce, command, events, balances }`,
   *     `kind` "user", "tick", "resume", "genesis", or "hidden" (another party's command, its origin
   *     stripped by the gateway); each balance with `before`, its value before this batch (or null).
   */
  function modelAt(recording, position) {
    const all = recording.batches ?? [];
    const last = all.length - 1;
    const at = position === undefined || position === null ? last : Math.max(-1, Math.min(last, position));
    const info = typeof recording.accountInfo === "function" ? recording.accountInfo : () => undefined;

    const balances = new Map(); // account -> balance, as of the batch being applied
    let previous = new Map();
    const requests = new Map(); // "epoch:id" -> request
    const quotes = new Map();
    const contracts = new Map();
    const positions = new Map();
    const trace = [];

    const k = (epoch, id) => `${epoch}:${id}`;
    /** The contract, made by its listing; null for a contract this recording never saw listed. */
    const contractFor = (epoch, id, listing = false) => {
      if (listing && !contracts.has(k(epoch, id))) {
        contracts.set(k(epoch, id), {
          id,
          epoch,
          description: null,
          oracle: null,
          trading_close: null,
          resolution_deadline: null,
          dispute_window: null,
          state: "Trading",
          dispute: null,
          proposal: null,
          ruling: null,
          bond_returned: null,
          positions: [],
        });
      }
      return contracts.get(k(epoch, id)) ?? null;
    };
    const positionFor = (epoch, id) => {
      if (!positions.has(k(epoch, id))) {
        positions.set(k(epoch, id), {
          id,
          epoch,
          contract: null,
          request: null,
          qty: null,
          yes_holder: null,
          yes_lock: null,
          no_holder: null,
          no_lock: null,
          settled: false,
          yes_payout: null,
          no_payout: null,
        });
      }
      return positions.get(k(epoch, id));
    };
    /** A position opened by the request's fill: its contract, qty, and the holder on `holderSide`'s side. */
    const fillPosition = (epoch, pid, request, holder, holderBuysYes) => {
      const p = positionFor(epoch, pid);
      const leg = request?.legs[0];
      if (request) p.request = request.id;
      if (leg) {
        p.contract = leg.contract;
        p.qty = leg.qty;
        const c = contracts.get(k(epoch, leg.contract));
        if (c && !c.positions.includes(pid)) c.positions.push(pid);
      }
      if (holder != null && holderBuysYes != null) p[holderBuysYes ? "yes_holder" : "no_holder"] = holder;
      return p;
    };

    for (let i = 0; i <= at; i++) {
      const b = all[i];
      const epoch = b.epoch ?? 0;
      const cmd = commandOf(recording, b);
      previous = new Map(balances);
      const rowBalances = (b.balances ?? []).map(({ account, balance }) => ({ account, balance, before: balances.get(account) ?? null }));
      for (const { account, balance } of b.balances ?? []) balances.set(account, balance);

      for (const e of b.events) {
        const name = variant(e.event);
        const d = body(e.event);
        const owner = e.audience && e.audience !== "Public" ? e.audience.Account : null;
        switch (name) {
          case "ContractListed": {
            const c = contractFor(epoch, d.contract, true);
            const listed = actionOf(cmd, "ListContract");
            if (listed && listed.id === d.contract) {
              Object.assign(c, {
                description: listed.description,
                oracle: listed.oracle,
                trading_close: listed.trading_close,
                resolution_deadline: listed.resolution_deadline,
                dispute_window: listed.dispute_window,
              });
            }
            break;
          }
          case "ContractClosed": {
            const c = contractFor(epoch, d.contract);
            if (c) c.state = "Closed";
            break;
          }
          case "OutcomeProposed": {
            const c = contractFor(epoch, d.contract);
            if (!c) break;
            c.state = { Proposed: { outcome: d.outcome, dispute_until: d.dispute_until } };
            // A Resume re-emits it with a later dispute_until: the proposal keeps its first time.
            c.proposal = { at: c.proposal?.outcome === d.outcome ? c.proposal.at : b.time, outcome: d.outcome, until: d.dispute_until };
            break;
          }
          case "Disputed": {
            const c = contractFor(epoch, d.contract);
            if (!c) break;
            const proposed = c.state?.Proposed?.outcome ?? c.proposal?.outcome ?? null;
            c.state = { Disputed: { proposed } };
            c.dispute = { disputer: d.disputer };
            break;
          }
          case "Ruled": {
            const c = contractFor(epoch, d.contract);
            if (c) c.ruling = { outcome: d.outcome, bond_returned: d.bond_returned };
            break;
          }
          case "ContractFinalized": {
            const c = contractFor(epoch, d.contract);
            if (c) c.state = { Final: d.outcome };
            break;
          }
          case "DisputeBondReturned": {
            const c = contractFor(epoch, d.contract);
            if (c) c.bond_returned = { disputer: d.disputer, amount: d.amount };
            break;
          }
          case "RequestOpened": {
            const submitted = actionOf(cmd, "SubmitRequest");
            let r = requests.get(k(epoch, d.request));
            if (!r) {
              r = {
                id: d.request,
                epoch,
                requester: submitted ? cmd.actor : null,
                state: "Open",
                response_deadline: d.response_deadline,
                legs: [],
                picks: [],
              };
              requests.set(k(epoch, d.request), r);
            }
            r.legs[d.leg] = {
              contract: d.contract,
              side: d.side,
              qty: d.qty,
              limit_price: submitted?.legs?.[d.leg]?.limit_price ?? null,
              quotes: [],
              selected: null,
            };
            break;
          }
          case "RequestPresented": {
            const r = requests.get(k(epoch, d.request));
            if (r) {
              r.state = { Presented: { accept_by: d.accept_by } };
              r.picks = d.picks;
              // Presented goes to the requester only: whoever hears it is the requester.
              if (r.requester == null && owner != null) r.requester = owner;
            }
            break;
          }
          case "RequestFilled": {
            const r = requests.get(k(epoch, d.request));
            if (r) {
              r.state = "Filled";
              if (r.requester == null && owner != null) r.requester = owner;
            }
            for (const pid of d.positions) fillPosition(epoch, pid, r, r?.requester ?? owner, r ? r.legs[0]?.side === "BuyYes" : null);
            break;
          }
          case "RequestCancelled":
          case "RequestExpired": {
            const r = requests.get(k(epoch, d.request));
            if (r) r.state = name === "RequestCancelled" ? "Cancelled" : "Expired";
            break;
          }
          case "RequestRejected": {
            const r = requests.get(k(epoch, d.request));
            if (r) r.state = { Rejected: d.cause };
            break;
          }
          case "QuoteSubmitted": {
            const quoted = actionOf(cmd, "SubmitQuote");
            const q = {
              id: d.quote,
              epoch,
              mm: owner ?? (quoted ? cmd.actor : null),
              request: d.request,
              leg: quoted && quoted.request === d.request ? quoted.leg : null,
              price: d.price,
              size: quoted && quoted.request === d.request ? quoted.size : null,
              expires_at: d.expires_at,
              state: "Active",
              reserved: null,
            };
            quotes.set(k(epoch, d.quote), q);
            const r = requests.get(k(epoch, d.request));
            const leg = r?.legs[q.leg ?? 0];
            if (leg && !leg.quotes.includes(q.id)) leg.quotes.push(q.id);
            break;
          }
          case "QuoteReplaced": {
            const q = quotes.get(k(epoch, d.old));
            if (q) q.state = "Replaced";
            break;
          }
          case "QuoteCancelled":
          case "QuoteExpired":
          case "QuoteLost":
          case "QuoteReleased":
          case "QuoteSelected": {
            const q = quotes.get(k(epoch, d.quote));
            if (!q) break;
            q.state = name.slice("Quote".length);
            if (name === "QuoteSelected") {
              const leg = requests.get(k(epoch, q.request))?.legs[q.leg ?? 0];
              if (leg) leg.selected = q.id;
            }
            break;
          }
          case "QuoteFilled": {
            const q = quotes.get(k(epoch, d.quote));
            if (q) q.state = "Filled";
            const r = q ? requests.get(k(epoch, q.request)) : null;
            // The maker takes the other side of the requester's.
            fillPosition(epoch, d.position, r, q?.mm ?? owner, r ? r.legs[0]?.side !== "BuyYes" : null);
            break;
          }
          case "PositionSettled": {
            // Settles a position this recording saw filled; one filled before it began stays unknown.
            const p = positions.get(k(epoch, d.position));
            if (!p) break;
            Object.assign(p, {
              yes_holder: d.yes_holder,
              no_holder: d.no_holder,
              yes_payout: d.yes_payout,
              no_payout: d.no_payout,
              settled: true,
            });
            break;
          }
          default:
          // Initialized, CommandRejected, Deposited, Withdrawn: no object moves (money is in `balances`).
        }
      }

      const kind = b.kind === "User" && !b.origin ? "hidden" : KIND[b.kind] ?? String(b.kind).toLowerCase();
      trace.push({
        key: k(epoch, b.seq),
        seq: b.seq,
        epoch,
        time: b.time,
        kind,
        actor: b.origin?.actor ?? null,
        nonce: b.origin?.nonce ?? null,
        command: cmd ? { User: { actor: cmd.actor, nonce: cmd.nonce, action: cmd.action } } : null,
        events: b.events,
        balances: rowBalances,
      });
    }

    const accounts = [...balances].map(([id, balance]) => {
      const who = info(id);
      return { id, name: who?.name ?? `#${id}`, role: who?.role ?? "account", balance };
    });
    accounts.sort((a, b) => a.id - b.id);
    const current = at >= 0 ? all[at] : null;
    return {
      position: at,
      count: all.length,
      seq: current?.seq ?? null,
      epoch: current?.epoch ?? 0,
      now: current?.time ?? null,
      viewer: recording.viewer ?? "admin",
      accounts,
      previous: at >= 0 ? previous : new Map(),
      requests: [...requests.values()],
      quotes: [...quotes.values()],
      contracts: [...contracts.values()],
      positions: [...positions.values()],
      trace,
    };
  }

  return { modelAt, variant, body };
})();
if (typeof module !== "undefined") module.exports = RfqModel;
