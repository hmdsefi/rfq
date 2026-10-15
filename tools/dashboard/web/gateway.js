// The WebSocket client of rfq-gateway (docs/specs/gateway.md §3). Plain script: it runs in the page
// (inlined) and in Node 22+ (`require`), using the global `WebSocket`. No dependencies.
//
//   const gw = RfqGateway.connect("ws://127.0.0.1:18080");
//   await gw.ready;                                    // connected, hello sent, clock measured
//   const { sent, result, batch } = await gw.command({ actor: 10, action: { Withdraw: { amount: 0 } } });
//
// It reconnects by itself with backoff and, on every reconnect, sends `hello` again with the last view.
// It makes no business decision: money, ids and times are the engine's, passed through.
const RfqGateway = (() => {
  /** How long a command may wait for its `result` or `error`. The gateway itself gives up on a silent engine after 3 s. */
  // Must outlast the gateway's 60 s hold on an unresolved nonce (gateway.md §6).
  const COMMAND_TIMEOUT_MS = 75_000;
  /** How long a `hello` may wait for its `welcome`. */
  const HELLO_TIMEOUT_MS = 5_000;
  /** Reconnect delays, in ms; the last one repeats. */
  const BACKOFF_MS = [250, 500, 1_000, 2_000, 4_000];
  /** Batches remembered by `seq`, so that a `result` can be given its batch. */
  const RECENT_BATCHES = 256;
  /** Engine status frames remembered, for `untilEngineUp`. */
  const RECENT_STATUSES = 64;
  /** Clock samples taken at connect: the one with the shortest round trip wins. */
  const CLOCK_SAMPLES = 5;

  // The gateway's sentence for a command that was offered when the engine stopped answering.
  const UNKNOWN_OUTCOME = /^outcome unknown:.*\bresend it with nonce (\d+)\b/;

  /**
   * Why a command did not get a `result`.
   * `kind`: `unknown_outcome` (the engine stopped answering after the command was sent; `nonce` is the
   * one to resend it with), `unavailable` (never offered), `refused` (any other `error` frame),
   * `closed` (the connection dropped or was never open; `nonce` when `sent` had arrived),
   * `timeout` (nothing came; `nonce` likewise).
   */
  class GatewayError extends Error {
    constructor(message, kind, extra = {}) {
      super(message);
      this.name = "GatewayError";
      this.kind = kind;
      Object.assign(this, extra);
    }
  }

  function errorFor(frame, entry, frameNo) {
    const unknown = UNKNOWN_OUTCOME.exec(frame.error);
    const kind = unknown ? "unknown_outcome" : frame.error === "engine unavailable" ? "unavailable" : "refused";
    const extra = { id: entry.id, frameNo, sent: entry.sent };
    if (unknown) extra.nonce = Number(unknown[1]);
    return new GatewayError(frame.error, kind, extra);
  }

  /** The error for a connection that went away (or an answer that never came) under `entry`. */
  function closedError(entry, why, kind = "closed") {
    const extra = { id: entry.id, sent: entry.sent };
    if (entry.sent) extra.nonce = entry.sent.nonce;
    return new GatewayError(why, kind, extra);
  }

  function connect(url, opts = {}) {
    const Socket = opts.WebSocket ?? globalThis.WebSocket;
    if (!Socket) throw new Error("no WebSocket in this environment (Node 22 or later, or a browser)");
    const listeners = new Map(); // type -> Set<fn>
    const pending = new Map(); // command id -> entry
    const helloWaiters = []; // FIFO: the gateway answers hellos in order
    const recentBatches = new Map(); // seq -> first frame of that seq
    const recentStatuses = []; // { n, engine }
    let ws = null;
    let isOpen = false;
    let closedByUser = false;
    let attempt = 0;
    let retryTimer = null;
    let frameNo = 0;
    let counter = 0;
    let view = opts.view ?? "admin";
    let engine = null;
    let offset = 0; // engine (gateway) wall clock minus this machine's, in ms
    let bestRtt = Infinity;
    let everOpen = false;

    const gw = {
      url,
      ready: null,
      get view() {
        return view;
      },
      get engine() {
        return engine;
      },
      get connected() {
        return isOpen;
      },
      get offset() {
        return offset;
      },
      /** How many frames have arrived, so far: a point in the stream to wait "after". */
      get frameCount() {
        return frameNo;
      },
      engineNow: () => Date.now() + offset,
      send,
      on,
      hello,
      command,
      syncClock,
      untilEngineUp,
      close,
    };

    /**
     * `fn(frame, { n, at, t })` for every frame of `type` (`"welcome"`, `"batch"`, …) or of any type
     * (`"*"`); `n` counts frames from 1, `at` is `Date.now()` and `t` is `performance.now()` at
     * receipt. `"conn"` instead gets `{ up, reconnected, attempt }` when the connection changes.
     * Returns the function that removes `fn`.
     */
    function on(type, fn) {
      if (!listeners.has(type)) listeners.set(type, new Set());
      listeners.get(type).add(fn);
      return () => listeners.get(type)?.delete(fn);
    }

    function emit(type, payload, meta) {
      for (const t of [type, "*"]) {
        if (t === "*" && type === "conn") continue;
        for (const fn of [...(listeners.get(t) ?? [])]) {
          try {
            fn(payload, meta);
          } catch (e) {
            console.error(`gateway listener for ${t} threw:`, e);
          }
        }
      }
    }

    function send(msg) {
      if (!isOpen) throw new GatewayError("not connected to the gateway", "closed");
      ws.send(JSON.stringify(msg));
    }

    /** Sets this connection's view (`"admin"` or `{ account }`); resolves with the `welcome`. */
    function hello(next = view) {
      view = next;
      return new Promise((resolve, reject) => {
        const waiter = { resolve, reject, sentAt: performance.now(), timer: null };
        waiter.timer = setTimeout(() => {
          const at = helloWaiters.indexOf(waiter);
          if (at >= 0) helloWaiters.splice(at, 1);
          reject(new GatewayError(`no welcome within ${HELLO_TIMEOUT_MS} ms`, "timeout"));
        }, HELLO_TIMEOUT_MS);
        try {
          send({ type: "hello", view: next });
        } catch (e) {
          clearTimeout(waiter.timer);
          reject(e);
          return;
        }
        helloWaiters.push(waiter);
      });
    }

    /**
     * Measures the engine clock against this machine's: `welcome.now` is the gateway's wall clock, which
     * is the engine's. Each hello gives one sample (the reply was written halfway through its round
     * trip); the sample with the shortest round trip is the most exact. Re-sends the current view.
     */
    async function syncClock(samples = CLOCK_SAMPLES) {
      bestRtt = Infinity; // measure afresh: the clocks may have drifted since the last time
      for (let i = 0; i < samples; i++) await hello(view);
      return offset;
    }

    /**
     * Sends a command and resolves with `{ id, sent, result, batch, rtt_ms }` once its `result`
     * arrives: `sent` carries the nonce, `batch` is the (first chunk of the) batch the engine made of
     * it, `rtt_ms` the round trip measured here. Rejects with a `GatewayError` on an `error` frame
     * for this command (see the class), a lost connection, or no answer within 30 s.
     */
    function command({ actor, action, nonce, id }) {
      id ??= `c${++counter}`;
      return new Promise((resolve, reject) => {
        if (pending.has(id)) {
          reject(new GatewayError(`command id ${id} is already in use`, "refused", { id }));
          return;
        }
        const msg = { type: "command", id, actor, action };
        if (nonce !== undefined) msg.nonce = nonce;
        const entry = { id, resolve, reject, sent: null, sentAt: performance.now(), timer: null };
        try {
          send(msg);
        } catch (e) {
          reject(closedError(entry, e.message));
          return;
        }
        entry.timer = setTimeout(() => {
          pending.delete(id);
          reject(closedError(entry, `no answer to ${id} within ${COMMAND_TIMEOUT_MS} ms`, "timeout"));
        }, COMMAND_TIMEOUT_MS);
        pending.set(id, entry);
      });
    }

    function settle(id) {
      const entry = pending.get(id);
      if (entry) {
        clearTimeout(entry.timer);
        pending.delete(id);
      }
      return entry;
    }

    function onFrame(frame, meta) {
      frameNo = meta.n;
      switch (frame.type) {
        case "batch":
          // The first chunk of a split batch is the one the `result` follows.
          if (!recentBatches.has(frame.seq)) {
            recentBatches.set(frame.seq, frame);
            if (recentBatches.size > RECENT_BATCHES) recentBatches.delete(recentBatches.keys().next().value);
          }
          break;
        case "status":
        case "welcome":
          engine = frame.engine;
          recentStatuses.push({ n: meta.n, engine: frame.engine });
          if (recentStatuses.length > RECENT_STATUSES) recentStatuses.shift();
          break;
        default:
      }
      // Listeners first: whoever awaits a command's promise then finds the recorder up to date.
      emit(frame.type, frame, meta);
      switch (frame.type) {
        case "welcome": {
          const waiter = helloWaiters.shift();
          if (!waiter) break;
          clearTimeout(waiter.timer);
          const rtt = meta.t - waiter.sentAt;
          if (rtt <= bestRtt) {
            bestRtt = rtt;
            offset = frame.now - (meta.at - rtt / 2);
          }
          waiter.resolve(frame);
          break;
        }
        case "sent": {
          const entry = pending.get(frame.id);
          if (entry) entry.sent = frame;
          break;
        }
        case "result": {
          const entry = settle(frame.id);
          // No entry: the late result of a command whose `error` said "outcome unknown".
          if (entry) {
            entry.resolve({
              id: frame.id,
              sent: entry.sent,
              result: frame,
              batch: recentBatches.get(frame.seq) ?? null,
              rtt_ms: meta.t - entry.sentAt,
            });
          }
          break;
        }
        case "error": {
          let entry;
          if (frame.id != null) {
            entry = settle(frame.id);
          } else if (frame.error.startsWith("bad request")) {
            // A frame the gateway could not read, so it cannot name the command. If exactly one command
            // is still waiting for its `sent`, it is that one: fail it now rather than at the timeout.
            const unsent = [...pending.values()].filter((p) => !p.sent);
            if (unsent.length === 1) entry = settle(unsent[0].id);
          }
          if (entry) entry.reject(errorFor(frame, entry, meta.n));
          break;
        }
        default:
      }
    }

    function open() {
      retryTimer = null;
      let socket;
      try {
        socket = new Socket(url);
      } catch (e) {
        // A bad URL never gets better.
        failReady(e);
        return;
      }
      ws = socket;
      socket.onopen = async () => {
        if (ws !== socket) return;
        isOpen = true;
        const reconnected = everOpen;
        everOpen = true;
        attempt = 0;
        try {
          await syncClock(CLOCK_SAMPLES);
        } catch (e) {
          // The connection dropped under the hello: onclose has handled it. A gateway that does not
          // answer a hello is no use: drop the connection so that it is tried afresh.
          if (!(e instanceof GatewayError)) console.error("gateway hello failed:", e);
          else if (e.kind === "timeout") socket.close();
          return;
        }
        emit("conn", { up: true, reconnected, attempt: 0 });
        readyOk(gw);
      };
      socket.onmessage = (ev) => {
        let frame;
        try {
          frame = JSON.parse(ev.data);
        } catch {
          console.error("gateway sent a frame that is not JSON:", String(ev.data).slice(0, 200));
          return;
        }
        onFrame(frame, { n: frameNo + 1, at: Date.now(), t: performance.now() });
      };
      socket.onclose = () => {
        if (ws !== socket) return;
        const wasOpen = isOpen;
        isOpen = false;
        engine = null;
        for (const waiter of helloWaiters.splice(0)) {
          clearTimeout(waiter.timer);
          waiter.reject(new GatewayError("connection closed", "closed"));
        }
        for (const entry of [...pending.values()]) {
          settle(entry.id);
          entry.reject(closedError(entry, "connection to the gateway lost"));
        }
        if (closedByUser) return;
        if (wasOpen) emit("conn", { up: false, reconnected: false, attempt });
        if (!everOpen && opts.retryInitial === false) {
          failReady(new GatewayError(`cannot connect to ${url} (is the gateway up?)`, "closed"));
          return;
        }
        const delay = BACKOFF_MS[Math.min(attempt, BACKOFF_MS.length - 1)];
        attempt++;
        retryTimer = setTimeout(open, delay);
      };
      // The error event carries nothing more than the close that follows it.
      socket.onerror = () => {};
    }

    let readyOk;
    let readyFail;
    let readyTimer = null;
    gw.ready = new Promise((resolve, reject) => {
      readyOk = (v) => {
        clearTimeout(readyTimer);
        resolve(v);
      };
      readyFail = reject;
    });
    // Nobody may be waiting: a failure is still reported through `ready`, never as an unhandled rejection.
    gw.ready.catch(() => {});
    function failReady(e) {
      clearTimeout(readyTimer);
      close();
      readyFail(e instanceof Error ? e : new Error(String(e)));
    }
    if (opts.connectTimeoutMs) {
      readyTimer = setTimeout(
        () => failReady(new GatewayError(`cannot connect to ${url} within ${opts.connectTimeoutMs} ms (is the gateway up?)`, "closed")),
        opts.connectTimeoutMs,
      );
    }

    /**
     * Resolves when the engine is `up` in a `status` or `welcome` frame that arrived after frame
     * number `after` (`gw.frameCount` at the time of interest, or a `GatewayError.frameNo`).
     */
    function untilEngineUp({ after = frameNo, timeoutMs = 60_000 } = {}) {
      return new Promise((resolve, reject) => {
        let off = () => {};
        const timer = setTimeout(() => {
          off();
          reject(new GatewayError(`the engine was not up again within ${timeoutMs / 1000} s`, "timeout"));
        }, timeoutMs);
        const done = () => {
          clearTimeout(timer);
          off();
          resolve();
        };
        if (recentStatuses.some((s) => s.n > after && s.engine === "up")) {
          done();
          return;
        }
        const check = (frame, meta) => {
          if (meta.n > after && frame.engine === "up") done();
        };
        const offs = [on("status", check), on("welcome", check)];
        off = () => offs.forEach((f) => f());
      });
    }

    function close() {
      closedByUser = true;
      clearTimeout(retryTimer);
      clearTimeout(readyTimer);
      for (const entry of [...pending.values()]) {
        settle(entry.id);
        entry.reject(closedError(entry, "the client closed the connection"));
      }
      for (const waiter of helloWaiters.splice(0)) {
        clearTimeout(waiter.timer);
        waiter.reject(new GatewayError("the client closed the connection", "closed"));
      }
      isOpen = false;
      try {
        ws?.close();
      } catch {
        // already closed
      }
    }

    open();
    return gw;
  }

  return { connect, GatewayError };
})();
if (typeof module !== "undefined") module.exports = RfqGateway;
