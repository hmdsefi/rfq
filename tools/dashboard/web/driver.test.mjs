// Tests of the gateway client and the driver against a scripted fake gateway (no network, no engine):
// the answers a command can get, the clock, reconnection, and above all the "outcome unknown"
// retry of docs/specs/gateway.md §6. Usage: node web/driver.test.mjs
import assert from "node:assert/strict";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const G = require("./gateway.js");
const S = require("./scenarios.js");

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

/**
 * A fake gateway server. Its `Socket` class stands in for `WebSocket`. `hello` is answered as the
 * real gateway does (a `welcome` and a `status`); every other message goes to `handler(msg, server)`,
 * which answers with `server.deliver(frame)`.
 */
function fakeServer(handler = () => {}) {
  const server = {
    engine: "up",
    skewMs: 0,
    sockets: [],
    received: [],
    replyDelayMs: 0,
    deliver(frame) {
      const socket = server.sockets.at(-1);
      socket.onmessage?.({ data: JSON.stringify(frame) });
    },
    /** The server drops the connection. */
    drop() {
      const socket = server.sockets.at(-1);
      socket.onclose?.({});
    },
    Socket: class {
      constructor(url) {
        this.url = url;
        server.sockets.push(this);
        setTimeout(() => this.onopen?.({}), 0);
      }
      send(text) {
        const msg = JSON.parse(text);
        server.received.push(msg);
        setTimeout(() => {
          if (msg.type === "hello") {
            server.deliver({ type: "welcome", view: msg.view, engine: server.engine, now: Date.now() + server.skewMs });
            server.deliver({ type: "status", engine: server.engine, last_seq: null });
          } else {
            handler(msg, server);
          }
        }, server.replyDelayMs);
      }
      close() {
        this.onclose?.({});
      }
    },
  };
  return server;
}

async function connected(handler, driverOpts) {
  const server = fakeServer(handler);
  const gw = G.connect("ws://fake", { WebSocket: server.Socket, connectTimeoutMs: 2_000 });
  await gw.ready;
  const driver = S.createDriver(gw, { startMarginMs: 0, graceMs: 30, engineWaitMs: 400, ...driverOpts });
  return { server, gw, driver };
}

// ---- frames -------------------------------------------------------------------------------------

const UNKNOWN = (nonce) =>
  `outcome unknown: the engine stopped answering after this command was sent; resend it with nonce ${nonce} to retry safely (a repeat is rejected as Duplicate)`;
const sent = (msg, nonce = msg.nonce) => ({ type: "sent", id: msg.id, actor: msg.actor, nonce });
const result = (msg, seq, latency_us = 100) => ({ type: "result", id: msg.id, seq, latency_us });
const batch = (seq, origin, events, kind = "User") => ({ type: "batch", seq, time: Date.now(), kind, origin, events, balances: [] });
const deposited = (msg) => ({ audience: "Public", event: { Deposited: { account: msg.action.Deposit.to, amount: 5 } } });
const duplicate = { audience: { Account: 1 }, event: { CommandRejected: { reason: "Duplicate" } } };
const status = (engine, last_seq = 9) => ({ type: "status", engine, last_seq });

/** A one-step scenario: the operator funds Alice. */
const funding = (extra = []) => ({
  name: "fake",
  kind: "brief",
  summary: "",
  steps: [{ at: 0, label: "dep", actor: 1, action: () => ({ Deposit: { to: 10, amount: 5 } }) }, ...extra],
  expect: ["Deposited"],
});

const isRetry = (msg, n = 1) => msg.id.endsWith(`.retry${n}`);

// ---- the client ---------------------------------------------------------------------------------

await test("a command resolves with sent, result and the batch the result follows, and its rtt", async () => {
  const { server, gw } = await connected((msg, s) => {
    s.deliver(sent(msg, 3));
    s.deliver(batch(20, { actor: msg.actor, nonce: 3 }, [duplicate]));
    s.deliver(result(msg, 20, 840));
  });
  const done = await gw.command({ actor: 5, action: { Withdraw: { amount: 0 } } });
  assert.equal(done.sent.nonce, 3);
  assert.equal(done.result.latency_us, 840);
  assert.equal(done.batch.seq, 20);
  assert.ok(done.rtt_ms >= 0);
  assert.deepEqual(server.received.at(-1), { type: "command", id: done.id, actor: 5, action: { Withdraw: { amount: 0 } } });
  gw.close();
});

await test("an explicit nonce is sent; none is sent otherwise", async () => {
  const { server, gw } = await connected((msg, s) => {
    s.deliver(sent(msg, msg.nonce ?? 1));
    s.deliver(result(msg, 1));
  });
  await gw.command({ actor: 5, action: { Withdraw: { amount: 0 } }, nonce: 9 });
  assert.equal(server.received.at(-1).nonce, 9);
  await gw.command({ actor: 5, action: { Withdraw: { amount: 0 } } });
  assert.equal("nonce" in server.received.at(-1), false);
  gw.close();
});

await test("an error frame for the command rejects it with the sentence and a kind", async () => {
  const sentences = {
    "engine unavailable": "unavailable",
    "ingress back-pressured, try again": "refused",
    "bad request: command too large for ingress (70000 bytes, limit 65536)": "refused",
    [UNKNOWN(12)]: "unknown_outcome",
  };
  const { gw } = await connected((msg, s) => {
    s.deliver({ type: "error", id: msg.id, error: msg.action.Withdraw.sentence });
  });
  for (const [sentence, kind] of Object.entries(sentences)) {
    await assert.rejects(
      gw.command({ actor: 5, action: { Withdraw: { sentence } } }),
      (e) => e instanceof G.GatewayError && e.message === sentence && e.kind === kind && (kind !== "unknown_outcome" || e.nonce === 12),
    );
  }
  gw.close();
});

await test("an unreadable frame (error with no id) fails the one command that has no `sent`, and no other", async () => {
  const { gw } = await connected((msg, s) => {
    if (msg.action.Withdraw.bad) s.deliver({ type: "error", id: null, error: "bad request: unknown variant" });
  });
  const bad = gw.command({ actor: 5, action: { Withdraw: { bad: true } } });
  await assert.rejects(bad, /bad request: unknown variant/);
  // Two commands waiting: it cannot say which, so neither is failed by it.
  let a = null;
  let b = null;
  gw.command({ actor: 5, action: { Withdraw: { bad: true } }, id: "x" }).then((v) => (a = v), (e) => (a = e));
  gw.command({ actor: 6, action: { Withdraw: { amount: 1 } }, id: "y" }).then((v) => (b = v), (e) => (b = e));
  await sleep(30);
  assert.equal(a, null);
  assert.equal(b, null);
  gw.close();
  await sleep(5);
  assert.equal(a.kind, "closed");
});

await test("a lost connection rejects what is waiting (with the nonce if `sent` came), and the client reconnects with the last view", async () => {
  const { server, gw } = await connected((msg, s) => {
    s.deliver(sent(msg, 4));
  });
  await gw.hello({ account: 10 });
  const events = [];
  gw.on("conn", (c) => events.push(c));
  const waiting = gw.command({ actor: 5, action: { Withdraw: { amount: 0 } } });
  await sleep(10);
  server.drop();
  await assert.rejects(waiting, (e) => e.kind === "closed" && e.nonce === 4);
  assert.equal(gw.connected, false);
  // The next command fails at once, not after a timeout.
  await assert.rejects(gw.command({ actor: 5, action: { Withdraw: { amount: 0 } } }), (e) => e.kind === "closed" && /not connected/.test(e.message));
  await sleep(450);
  assert.equal(gw.connected, true);
  assert.equal(server.sockets.length, 2);
  assert.deepEqual(events.map((c) => [c.up, c.reconnected]), [[false, false], [true, true]]);
  const hellos = server.received.filter((m) => m.type === "hello");
  assert.deepEqual(hellos.at(-1).view, { account: 10 });
  assert.deepEqual(gw.view, { account: 10 });
  gw.close();
});

await test("the engine clock: the offset is the gateway's now against the middle of the round trip, from the quickest sample", async () => {
  const server = fakeServer();
  server.skewMs = 5_000;
  const gw = G.connect("ws://fake", { WebSocket: server.Socket, connectTimeoutMs: 2_000 });
  await gw.ready;
  assert.ok(Math.abs(gw.offset - 5_000) < 25, `offset ${gw.offset}`);
  assert.ok(Math.abs(gw.engineNow() - Date.now() - 5_000) < 25);
  // A slow answer carries a worse estimate; the offset is re-measured from scratch by syncClock.
  server.skewMs = -3_000;
  await gw.syncClock(3);
  assert.ok(Math.abs(gw.offset + 3_000) < 25, `offset ${gw.offset}`);
  gw.close();
});

await test("untilEngineUp waits for an `up` after the given frame, and gives up", async () => {
  const { server, gw } = await connected();
  const after = gw.frameCount;
  let up = false;
  const waiting = gw.untilEngineUp({ after, timeoutMs: 500 }).then(() => (up = true));
  server.deliver(status("down"));
  await sleep(5);
  assert.equal(up, false);
  server.deliver(status("up"));
  await waiting;
  // An `up` that already came after the frame counts; one before it does not.
  await gw.untilEngineUp({ after, timeoutMs: 50 });
  await assert.rejects(gw.untilEngineUp({ after: gw.frameCount, timeoutMs: 30 }), /not up again/);
  gw.close();
});

await test("connect fails with a sentence when the gateway cannot be reached", async () => {
  class Refused {
    constructor() {
      setTimeout(() => this.onclose?.({}), 0);
    }
  }
  await assert.rejects(G.connect("ws://nowhere", { WebSocket: Refused, retryInitial: false }).ready, /cannot connect to ws:\/\/nowhere/);
  await assert.rejects(G.connect("ws://nowhere", { WebSocket: Refused, connectTimeoutMs: 60 }).ready, /cannot connect to ws:\/\/nowhere within 60 ms/);
});

// ---- the driver ---------------------------------------------------------------------------------

await test("a plain run: the label is the result's seq, the run's own events pass, perf has the command", async () => {
  const { gw, driver } = await connected((msg, s) => {
    s.deliver(sent(msg, 3));
    s.deliver(batch(40, { actor: msg.actor, nonce: 3 }, [deposited(msg)]));
    s.deliver(result(msg, 40, 700));
  });
  const run = await driver.run(funding());
  assert.equal(run.pass, true, run.detail);
  assert.equal(run.labels.dep, 40);
  assert.deepEqual(run.commands.map((c) => [c.step, c.seq, c.nonce, c.latency_us, c.recovered]), [["dep", 40, 3, 700, false]]);
  assert.equal(run.perf.commands, 1);
  assert.deepEqual(run.perf.latency_us, { n: 1, p50: 700, p95: 700, max: 700 });
  assert.equal(run.own.length, 1);
  assert.equal(run.contract, run.t0);
  assert.equal(run.accounts.alice, run.t0 * 100 + 10);
  gw.close();
});

await test("another run's events are not this run's: the same event for someone else's account is ignored", async () => {
  const { server, gw, driver } = await connected((msg, s) => {
    s.deliver(sent(msg, 3));
    // Someone else's deposit arrives first (another client of the node), then ours.
    s.deliver(batch(39, { actor: 1, nonce: 2 }, [{ audience: "Public", event: { Deposited: { account: 12345, amount: 5 } } }]));
    s.deliver(batch(40, { actor: msg.actor, nonce: 3 }, [deposited(msg)]));
    s.deliver(result(msg, 40));
  });
  const run = await driver.run(funding());
  assert.equal(run.pass, true, run.detail);
  assert.deepEqual(run.own.map((o) => o.seq), [40]);
  void server;
  gw.close();
});

await test("a refused command fails the run with the sentence, at once", async () => {
  const { gw, driver } = await connected((msg, s) => s.deliver({ type: "error", id: msg.id, error: "engine unavailable" }));
  const started = Date.now();
  const run = await driver.run(funding());
  assert.equal(run.pass, false);
  assert.match(run.detail, /dep \(operator Deposit\): engine unavailable/);
  assert.ok(Date.now() - started < 1_000);
  assert.equal(run.recoveries.length, 0);
  gw.close();
});

await test("unknown outcome, applied before: the engine rejects the retry as Duplicate, and the label is the original's seq", async () => {
  const { gw, driver } = await connected((msg, s) => {
    if (isRetry(msg)) {
      assert.equal(msg.nonce, 7, "the retry carries the same nonce");
      s.deliver(sent(msg));
      s.deliver(batch(41, { actor: msg.actor, nonce: 7 }, [duplicate]));
      s.deliver(result(msg, 41));
      return;
    }
    s.deliver(sent(msg, 7));
    setTimeout(() => {
      s.deliver({ type: "error", id: msg.id, error: UNKNOWN(7) });
      s.deliver(status("down"));
      setTimeout(() => {
        s.deliver(status("up"));
        // The engine applied the original after all: its batch and its late result.
        s.deliver(batch(40, { actor: msg.actor, nonce: 7 }, [deposited(msg)]));
        s.deliver(result(msg, 40, 4_000_000));
      }, 20);
    }, 10);
  });
  const run = await driver.run(funding());
  assert.equal(run.pass, true, run.detail);
  assert.equal(run.labels.dep, 40);
  assert.deepEqual(run.recoveries.map((r) => [r.step, r.nonce, r.seq]), [["dep", 7, 40]]);
  // The Duplicate proves only that the nonce was taken; the original's own batch says how.
  assert.equal(run.recoveries[0].outcome, "nonce 7 taken once (Duplicate on retry); original: applied");
  assert.ok(
    run.log.some((l) => l.includes("outcome recovered: nonce 7 taken once (Duplicate on retry); original: applied")),
    run.log.join("\n"),
  );
  assert.equal(run.commands[0].recovered, true);
  // A command that waited out an outage is not a latency sample.
  assert.equal(run.perf.commands, 0);
  // The retry's Duplicate is a recovery artifact, not the run's own event (ruling c): the deposit is
  // there once. The timeline still shows the retry's batch, with the retry as its command.
  assert.deepEqual(run.own.map((o) => o.seq), [40]);
  assert.deepEqual(run.batches, [40, 41]);
  assert.equal(driver.commandAt(41).id, `${run.id}.1.retry1`);
  assert.equal(driver.commandAt(41).nonce, 7);
  assert.equal(driver.commandAt(40).id, `${run.id}.1`, "the original's late result names its batch");
  gw.close();
});

await test("unknown outcome, applied before, but its batch was never heard: said so, the label is not made up, the verdict stays honest", async () => {
  const { gw, driver } = await connected((msg, s) => {
    if (isRetry(msg)) {
      s.deliver(sent(msg));
      s.deliver(batch(41, { actor: msg.actor, nonce: 7 }, [duplicate]));
      s.deliver(result(msg, 41));
      return;
    }
    s.deliver(sent(msg, 7));
    setTimeout(() => {
      s.deliver({ type: "error", id: msg.id, error: UNKNOWN(7) });
      s.deliver(status("down"));
      setTimeout(() => s.deliver(status("up")), 20);
    }, 10);
  });
  const run = await driver.run(funding());
  assert.equal(run.pass, false);
  assert.match(run.detail, /expected event Deposited/);
  assert.equal("dep" in run.labels, false);
  assert.equal(run.recoveries[0].seq, null);
  assert.equal(
    run.recoveries[0].outcome,
    "nonce 7 taken once (Duplicate on retry); original not heard — whether it was applied is unknown",
  );
  gw.close();
});

await test("a step that needs the label of a command whose batch was never heard fails the run, not the process", async () => {
  const needs = { at: 0, label: "again", actor: 1, action: (c) => ({ CancelQuote: { quote: c.quote("dep") } }) };
  const { gw, driver } = await connected((msg, s) => {
    if (isRetry(msg)) {
      s.deliver(sent(msg));
      s.deliver(batch(41, { actor: msg.actor, nonce: 7 }, [duplicate]));
      s.deliver(result(msg, 41));
      return;
    }
    s.deliver(sent(msg, 7));
    setTimeout(() => {
      s.deliver({ type: "error", id: msg.id, error: UNKNOWN(7) });
      s.deliver(status("down"));
      setTimeout(() => s.deliver(status("up")), 20);
    }, 10);
  });
  const run = await driver.run(funding([needs]));
  assert.equal(run.pass, false);
  assert.match(run.detail, /step again .*label dep has no seq yet/);
  gw.close();
});

await test("unknown outcome, not applied: the retry is applied, and the label is its seq", async () => {
  const { gw, driver } = await connected((msg, s) => {
    if (isRetry(msg)) {
      s.deliver(sent(msg));
      s.deliver(batch(42, { actor: msg.actor, nonce: 7 }, [deposited(msg)]));
      s.deliver(result(msg, 42));
      return;
    }
    s.deliver(sent(msg, 7));
    setTimeout(() => {
      s.deliver({ type: "error", id: msg.id, error: UNKNOWN(7) });
      s.deliver(status("down"));
      setTimeout(() => s.deliver(status("up")), 20);
    }, 10);
  });
  const run = await driver.run(funding());
  assert.equal(run.pass, true, run.detail);
  assert.equal(run.labels.dep, 42);
  assert.equal(run.recoveries[0].outcome, "retry applied");
  assert.ok(run.log.some((l) => l.includes("outcome recovered: retry applied")), run.log.join("\n"));
  gw.close();
});

const insufficient = { audience: { Account: 1 }, event: { CommandRejected: { reason: "InsufficientFunds" } } };

await test("unknown outcome, the original was rejected (it took the nonce): the retry's Duplicate is never called applied", async () => {
  const { gw, driver } = await connected((msg, s) => {
    if (isRetry(msg)) {
      s.deliver(sent(msg));
      s.deliver(batch(41, { actor: msg.actor, nonce: 7 }, [duplicate]));
      s.deliver(result(msg, 41));
      return;
    }
    s.deliver(sent(msg, 7));
    setTimeout(() => {
      s.deliver({ type: "error", id: msg.id, error: UNKNOWN(7) });
      s.deliver(status("down"));
      setTimeout(() => {
        s.deliver(status("up"));
        s.deliver(batch(40, { actor: msg.actor, nonce: 7 }, [insufficient]));
        s.deliver(result(msg, 40));
      }, 20);
    }, 10);
  });
  const run = await driver.run(funding());
  assert.equal(run.pass, false);
  assert.equal(run.labels.dep, 40, "the original's batch is the step's");
  assert.equal(run.recoveries[0].outcome, "nonce 7 taken once (Duplicate on retry); original: rejected: InsufficientFunds");
  assert.ok(!run.log.some((l) => /recovered: .*applied/.test(l)), run.log.join("\n"));
  gw.close();
});

await test("unknown outcome, the retry is rejected (not as Duplicate): said so, never applied", async () => {
  const { gw, driver } = await connected((msg, s) => {
    if (isRetry(msg)) {
      s.deliver(sent(msg));
      s.deliver(batch(42, { actor: msg.actor, nonce: 7 }, [insufficient]));
      s.deliver(result(msg, 42));
      return;
    }
    s.deliver(sent(msg, 7));
    setTimeout(() => {
      s.deliver({ type: "error", id: msg.id, error: UNKNOWN(7) });
      s.deliver(status("down"));
      setTimeout(() => s.deliver(status("up")), 20);
    }, 10);
  });
  const run = await driver.run(funding());
  assert.equal(run.pass, false);
  assert.equal(run.labels.dep, 42);
  assert.equal(run.recoveries[0].outcome, "retry rejected: InsufficientFunds");
  assert.ok(run.log.some((l) => l.includes("outcome recovered: retry rejected: InsufficientFunds")), run.log.join("\n"));
  gw.close();
});

await test("unknown outcome, and the original's batch answers the resend (whichever batch comes first): the label is that batch", async () => {
  const { gw, driver } = await connected((msg, s) => {
    if (isRetry(msg)) {
      s.deliver(sent(msg));
      s.deliver(batch(40, { actor: msg.actor, nonce: 7 }, [deposited(msg)]));
      s.deliver(result(msg, 40));
      s.deliver(batch(41, { actor: msg.actor, nonce: 7 }, [duplicate]));
      return;
    }
    s.deliver(sent(msg, 7));
    setTimeout(() => {
      s.deliver({ type: "error", id: msg.id, error: UNKNOWN(7) });
      s.deliver(status("down"));
      setTimeout(() => s.deliver(status("up")), 20);
    }, 10);
  });
  const run = await driver.run(funding());
  assert.equal(run.pass, true, run.detail);
  assert.equal(run.labels.dep, 40);
  // The retry's own Duplicate came after the result that answered it: still not the run's own event.
  assert.deepEqual(run.own.map((o) => o.seq), [40]);
  gw.close();
});

await test("a recovery artifact can't satisfy an expect: the retry's Duplicate is not the run's own event", async () => {
  // Applied before the outage, but the original's batch was never heard here: all there is, is the
  // retry's Duplicate. A scenario that expects a Duplicate must not pass on it.
  const { gw, driver } = await connected((msg, s) => {
    if (isRetry(msg)) {
      s.deliver(sent(msg));
      s.deliver(batch(41, { actor: msg.actor, nonce: 7 }, [duplicate]));
      s.deliver(result(msg, 41));
      return;
    }
    s.deliver(sent(msg, 7));
    setTimeout(() => {
      s.deliver({ type: "error", id: msg.id, error: UNKNOWN(7) });
      s.deliver(status("down"));
      setTimeout(() => s.deliver(status("up")), 20);
    }, 10);
  });
  const run = await driver.run({ ...funding(), expect: ["CommandRejected(Duplicate)"] });
  assert.equal(run.pass, false);
  assert.match(run.detail, /expected event CommandRejected\(Duplicate\)/);
  assert.deepEqual(run.own, []);
  assert.deepEqual(run.batches, [41]);
  gw.close();
});

await test("unknown outcome twice: the second retry works; the run's log says both", async () => {
  const { gw, driver } = await connected((msg, s) => {
    const retry2 = isRetry(msg, 2);
    if (retry2) {
      s.deliver(sent(msg));
      s.deliver(batch(43, { actor: msg.actor, nonce: 7 }, [deposited(msg)]));
      s.deliver(result(msg, 43));
      return;
    }
    s.deliver(sent(msg, 7));
    setTimeout(() => {
      s.deliver({ type: "error", id: msg.id, error: UNKNOWN(7) });
      s.deliver(status("down"));
      setTimeout(() => s.deliver(status("up")), 20);
    }, 10);
  });
  const run = await driver.run(funding());
  assert.equal(run.pass, true, run.detail);
  assert.equal(run.labels.dep, 43);
  assert.equal(run.log.filter((l) => l.includes("outcome unknown")).length, 2);
  gw.close();
});

await test("unknown outcome and the engine never comes back: the run fails, it does not hang", async () => {
  const { gw, driver } = await connected((msg, s) => {
    s.deliver(sent(msg, 7));
    setTimeout(() => {
      s.deliver({ type: "error", id: msg.id, error: UNKNOWN(7) });
      s.deliver(status("down"));
    }, 10);
  }, { engineWaitMs: 100 });
  const started = Date.now();
  const run = await driver.run(funding());
  assert.equal(run.pass, false);
  assert.match(run.detail, /was not up again within/);
  assert.ok(Date.now() - started < 1_500);
  gw.close();
});

await test("unknown outcome on a replay (explicit nonce): the nonce is not the step's own, so no batch is looked up for it", async () => {
  // The ordinary step is nonce 7 (seq 40). The replay step resends nonce 7 and is rejected as Duplicate,
  // however often: its answer is its own batch, not the batch of the command that owns the nonce.
  const replay = {
    at: 0,
    label: "replay",
    actor: 1,
    nonce: "reuse",
    action: () => ({ Deposit: { to: 10, amount: 5 } }),
  };
  let originals = 0;
  const { gw, driver } = await connected((msg, s) => {
    if (msg.id.endsWith(".1")) {
      s.deliver(sent(msg, 7));
      s.deliver(batch(40, { actor: msg.actor, nonce: 7 }, [deposited(msg)]));
      s.deliver(result(msg, 40));
    } else if (isRetry(msg)) {
      assert.equal(msg.nonce, 7);
      s.deliver(sent(msg));
      s.deliver(batch(42, { actor: msg.actor, nonce: 7 }, [duplicate]));
      s.deliver(result(msg, 42));
    } else {
      originals++;
      assert.equal(msg.nonce, 7, "reuse sends the last ordinary nonce");
      s.deliver(sent(msg, 7));
      setTimeout(() => {
        s.deliver({ type: "error", id: msg.id, error: UNKNOWN(7) });
        s.deliver(status("down"));
        setTimeout(() => s.deliver(status("up")), 20);
      }, 10);
    }
  });
  const run = await driver.run(funding([replay]));
  assert.equal(originals, 1);
  assert.equal(run.pass, true, run.detail);
  assert.equal(run.labels.dep, 40);
  assert.equal(run.labels.replay, 42);
  assert.match(run.recoveries[0].outcome, /^rejected by design either way/);
  gw.close();
});

await test("the driver refuses a connection in an account view: it could not see the run's events", async () => {
  const { gw, driver } = await connected();
  await gw.hello({ account: 10 });
  await assert.rejects(driver.run(funding()), /need the admin view/);
  await assert.rejects(driver.runMany(["happy_path"], 1, 1), /need the admin view/);
  gw.close();
});

await test("runMany: bad arguments are refused; t0 is distinct for runs started together", async () => {
  const t0s = [];
  const { gw, driver } = await connected((msg, s) => {
    s.deliver(sent(msg, 3));
    s.deliver(batch(40 + t0s.length, { actor: msg.actor, nonce: 3 }, [deposited(msg)]));
    s.deliver(result(msg, 40 + t0s.length));
  }, { onRunStart: (run) => t0s.push(run.t0) });
  await assert.rejects(driver.runMany(["nope"], 1, 1), /no scenario named nope/);
  await assert.rejects(driver.runMany(["happy_path"], 0, 1), /n must be/);
  await assert.rejects(driver.runMany(["happy_path"], 1, 0), /k must be/);
  await assert.rejects(driver.runMany([], 1, 1), /no scenarios/);
  const scenario = funding();
  const many = await driver.runMany([scenario], 6, 6);
  assert.equal(many.runs.length, 6);
  assert.equal(new Set(t0s).size, 6, `t0s ${t0s}`);
  assert.deepEqual([...t0s].sort((a, b) => a - b), t0s.slice().sort((a, b) => a - b));
  assert.ok(Math.max(...t0s) - Math.min(...t0s) >= 5);
  assert.equal(new Set(many.runs.map((r) => r.accounts.alice)).size, 6);
  assert.equal(many.perf.runs, 6);
  assert.equal(many.perf.commands, 6);
  gw.close();
});

// ---- run sets ride through an outage: each next run waits for the engine ------------------------

/**
 * A server whose engine goes down when the first run of a set ends and, unless `backAfterMs` is null,
 * comes back that much later. While down it refuses every command, as the gateway does.
 */
async function outageAfterFirstRun({ backAfterMs, engineWaitMs }) {
  let engine = "up";
  let refused = 0;
  const answer = answering();
  const server = fakeServer((msg, s) => {
    if (engine !== "up") {
      refused++;
      s.deliver({ type: "error", id: msg.id, error: "engine unavailable" });
      return;
    }
    answer(msg, s);
  });
  const gw = G.connect("ws://fake", { WebSocket: server.Socket, connectTimeoutMs: 2_000 });
  await gw.ready;
  let ended = 0;
  const driver = S.createDriver(gw, {
    startMarginMs: 0,
    graceMs: 30,
    engineWaitMs,
    onRunEnd: () => {
      if (++ended !== 1) return;
      engine = "down";
      server.deliver(status("down"));
      if (backAfterMs != null) {
        setTimeout(() => {
          engine = "up";
          server.deliver(status("up"));
        }, backAfterMs);
      }
    },
  });
  return { server, gw, driver, refused: () => refused };
}

await test("run sets: a run that would start while the engine is down waits for it instead of failing", async () => {
  const { server, gw, driver, refused } = await outageAfterFirstRun({ backAfterMs: 150, engineWaitMs: 2_000 });
  const out = await driver.runMany([funding()], 3, 1);
  assert.equal(out.passed, 3, out.runs.map((r) => r.detail).join("; "));
  assert.equal(refused(), 0, "nothing was sent while the engine was down");
  assert.match(out.runs[1].log[0], /^the engine was down: waited \d+ ms for it before starting$/);
  assert.equal(out.runs[2].log.some((l) => l.includes("waited")), false, "up again: the third did not wait");
  assert.equal(server.received.filter((m) => m.type === "command").length, 3);
  gw.close();
});

await test("run sets: the wait is bounded; past it the runs fail without sending anything, and the set ends", async () => {
  const { server, gw, driver } = await outageAfterFirstRun({ backAfterMs: null, engineWaitMs: 100 });
  const started = Date.now();
  const out = await driver.runMany([funding()], 3, 1);
  assert.ok(Date.now() - started < 1_000, `one wait, not one per run: took ${Date.now() - started} ms`);
  assert.deepEqual(out.runs.map((r) => r.pass), [true, false, false]);
  for (const r of out.runs.slice(1)) assert.match(r.detail, /^not started: the engine was not up again within 0\.1 s$/);
  assert.equal(server.received.filter((m) => m.type === "command").length, 1, "only the first run's command");
  gw.close();
});

await test("run sets: a Stop ends the wait for the engine at once; the run never starts", async () => {
  const { gw, driver } = await outageAfterFirstRun({ backAfterMs: null, engineWaitMs: 10_000 });
  const started = Date.now();
  const many = driver.runMany([funding()], 3, 1);
  await sleep(150);
  driver.stop();
  const out = await many;
  assert.ok(Date.now() - started < 1_000, `took ${Date.now() - started} ms`);
  assert.deepEqual([out.stopped, out.runs.length, out.runs[0].pass], [true, 1, true]);
  gw.close();
});

// ---- one driver per page: progress, stop, dispose, the clock (ruling b) ---------------------------

/** Answers every command at once: nonce 3, a deposit batch at the next seq, its result. */
function answering() {
  let seq = 100;
  return (msg, s) => {
    seq++;
    s.deliver(sent(msg, msg.nonce ?? 3));
    s.deliver(batch(seq, { actor: msg.actor, nonce: msg.nonce ?? 3 }, [deposited(msg)]));
    s.deliver(result(msg, seq));
  };
}

/** A two-command scenario whose second command waits `later` ms. */
const twoSteps = (later) => ({
  ...funding(),
  steps: [
    { at: 0, label: "dep", actor: 1, action: () => ({ Deposit: { to: 10, amount: 5 } }) },
    { at: later, label: "dep2", actor: 1, action: () => ({ Deposit: { to: 10, amount: 6 } }) },
  ],
});

await test("progress: a run says which step it is on, of how many, and the batches that are its own", async () => {
  const seen = [];
  const { gw, driver } = await connected(answering(), { onStep: (run) => seen.push([run.step, run.steps]) });
  const run = await driver.run({ ...twoSteps(10), steps: [...twoSteps(10).steps, { at: 20, wait: true }] });
  assert.equal(run.pass, true, run.detail);
  assert.deepEqual(seen, [[1, 3], [2, 3], [3, 3]]);
  assert.deepEqual(run.batches, [run.labels.dep, run.labels.dep2]);
  // Each batch's command, as the page sent it (the model joins on it): actor, nonce, the whole action.
  const c = driver.commandAt(run.labels.dep2);
  assert.deepEqual([c.run, c.step, c.actor, c.nonce], [run.id, "dep2", 1, 3]);
  assert.deepEqual(c.action, { Deposit: { to: run.t0 * 100 + 10, amount: 6 } });
  assert.equal(driver.commandAt(99_999), undefined);
  gw.close();
});

await test("stop: the runs in progress send nothing more and say so; runs not started never start", async () => {
  const { server, gw, driver } = await connected(answering());
  const many = driver.runMany([twoSteps(300)], 3, 1);
  await sleep(80);
  const before = server.received.filter((m) => m.type === "command").length;
  assert.equal(before, 1);
  driver.stop();
  const out = await many;
  await sleep(350);
  assert.equal(server.received.filter((m) => m.type === "command").length, before, "nothing sent after Stop");
  assert.equal(out.stopped, true);
  assert.equal(out.runs.length, 1, "only the run that had started");
  assert.equal(out.runs[0].pass, false);
  assert.equal(out.runs[0].stopped, true);
  assert.match(out.runs[0].detail, /stopped/);
  // The driver stays usable after a Stop.
  const again = await driver.run(funding());
  assert.equal(again.pass, true, again.detail);
  gw.close();
});

await test("dispose: the driver lets go of the connection, stops its runs, and a new driver can take it", async () => {
  const { server, gw, driver } = await connected(answering());
  assert.throws(() => S.createDriver(gw), /already has a driver/);
  const running = driver.run(twoSteps(300));
  await sleep(80);
  driver.dispose();
  const run = await running;
  assert.equal(run.stopped, true);
  const frames = driver.recorder.frames.length;
  server.deliver(batch(500, null, [], "Tick"));
  assert.equal(driver.recorder.frames.length, frames, "a disposed driver records nothing more");
  await assert.rejects(driver.run(funding()), /disposed/);
  const next = S.createDriver(gw, { startMarginMs: 0, graceMs: 30 });
  const ok = await next.run(funding());
  assert.equal(ok.pass, true, ok.detail);
  next.dispose();
  gw.close();
});

await test("the clock is measured again only when no run is active: a run in progress keeps its schedule", async () => {
  const { server, gw, driver } = await connected(answering());
  const hellos = () => server.received.filter((m) => m.type === "hello").length;
  const first = driver.run(twoSteps(250));
  await sleep(60);
  const during = hellos();
  const second = await driver.run(funding());
  assert.equal(second.pass, true, second.detail);
  assert.equal(hellos(), during, "no hello (no clock sample) while another run was active");
  assert.equal((await first).pass, true);
  await driver.run(funding());
  assert.equal(hellos(), during + 3, "an idle driver measures the clock afresh before a run");
  gw.close();
});

await test("stop while the driver waits for the engine to be up: the wait ends at once, nothing is sent", async () => {
  const server = fakeServer(answering());
  server.engine = "down";
  const gw = G.connect("ws://fake", { WebSocket: server.Socket, connectTimeoutMs: 2_000 });
  await gw.ready;
  const driver = S.createDriver(gw, { startMarginMs: 0, graceMs: 30, engineWaitMs: 10_000 });
  const started = Date.now();
  const many = driver.runMany(["happy_path"], 3, 1);
  const one = driver.run(funding());
  await sleep(50);
  driver.stop();
  const out = await many;
  assert.ok(Date.now() - started < 1_000, `took ${Date.now() - started} ms`);
  assert.deepEqual([out.stopped, out.runs.length, out.pass], [true, 0, false]);
  await assert.rejects(one, (e) => e.name === "Stopped");
  assert.equal(server.received.filter((m) => m.type === "command").length, 0);
  // The driver is still usable once the engine is up.
  server.engine = "up";
  server.deliver(status("up"));
  const ok = await driver.run(funding());
  assert.equal(ok.pass, true, ok.detail);
  gw.close();
});

await test("commandAt is keyed by (epoch, seq): a journal reset's seq 40 is not the old seq 40's command", async () => {
  let seq = 39;
  const { server, gw, driver } = await connected((msg, s) => {
    seq++;
    s.deliver(sent(msg, 3));
    s.deliver(batch(seq, { actor: msg.actor, nonce: 3 }, [deposited(msg)]));
    s.deliver(result(msg, seq));
  });
  const first = await driver.run(funding());
  assert.equal(driver.commandAt(40, 0).id, `${first.id}.1`);
  // `docker compose down -v`: a fresh journal; its seqs start again.
  server.deliver(batch(2, null, [], "Tick"));
  assert.equal(driver.recorder.epoch, 1);
  seq = 39;
  const second = await driver.run(funding());
  assert.equal(driver.commandAt(40, 1).id, `${second.id}.1`);
  assert.equal(driver.commandAt(40, 0).id, `${first.id}.1`, "the old epoch keeps its own");
  assert.equal(driver.commandAt(40).id, `${second.id}.1`, "no epoch: the current one");
  assert.equal(driver.commandAt(40, 2), undefined);
  gw.close();
});

console.log(failed ? `${failed} FAILED` : "all gateway client and driver tests passed");
process.exit(failed ? 1 : 0);
