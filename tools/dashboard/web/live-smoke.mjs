#!/usr/bin/env node
// Runs the dashboard's 14 scenarios against the live stack, as their participants, through the real
// path: this script -> WebSocket -> rfq-gateway -> Aeron -> rfq-node (Docker) and back.
//
//   docker compose up -d aeron engine gateway
//   node tools/dashboard/web/live-smoke.mjs [ws://127.0.0.1:18080] [--runs 20] [--concurrency 4] [--mix N] [--verbose]
//
// First all 14 scenarios once, one after the other, each judged PASS or FAIL against its expected
// events with its latency percentiles; then `happy_path` x20 at concurrency 4 (every run scoped to
// its own accounts and market, so none sees another's events), with the aggregate numbers. Exits 0
// only if every run passed. `--mix N` adds a third part: all 14 scenarios N times each, at the same
// concurrency, so that runs of different scenarios overlap on one engine. `--verbose` prints every
// run's log. Needs Node 22 or later (the global WebSocket).
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const { connect } = require("./gateway.js");
const { createDriver } = require("./scenarios.js");

const args = process.argv.slice(2);
const flag = (name, fallback) => {
  const at = args.indexOf(`--${name}`);
  return at >= 0 ? Number(args[at + 1]) : fallback;
};
const url = args.find((a) => /^wss?:\/\//.test(a)) ?? process.env.RFQ_GATEWAY_URL ?? "ws://127.0.0.1:18080";
const RUNS = flag("runs", 20);
const CONCURRENCY = flag("concurrency", 4);
const MIX = flag("mix", 0);
const VERBOSE = args.includes("--verbose");

const ms = (us) => (us == null ? "-" : (us / 1000).toFixed(2));
const msOf = (v) => (v == null ? "-" : v.toFixed(2));
const secs = (wall) => (wall / 1000).toFixed(1);

function latencyLine(perf) {
  const l = perf.latency_us;
  const r = perf.rtt_ms;
  return `latency p50 ${ms(l.p50)} p95 ${ms(l.p95)} max ${ms(l.max)} ms | round trip p50 ${msOf(r.p50)} p95 ${msOf(r.p95)} max ${msOf(r.max)} ms`;
}

function show(run) {
  if (run.pass) {
    console.log(`PASS ${run.name.padEnd(36)} ${String(run.perf.commands).padStart(2)} commands, ${latencyLine(run.perf)}, ${secs(run.wall_ms)} s`);
  } else {
    console.log(`FAIL ${run.name}: ${run.detail}`);
  }
  if (!run.pass || VERBOSE) for (const line of run.log) console.log(`       ${line}`);
  if (!run.pass) {
    const name = (o) => (typeof o.event === "string" ? o.event : Object.keys(o.event)[0]);
    console.log(`       own events: ${run.own.map((o) => `${o.seq}:${name(o)}`).join(" ")}`);
  }
}

async function main() {
  console.log(`live smoke against ${url}`);
  const gw = connect(url, { connectTimeoutMs: 10_000 });
  await gw.ready;
  console.log(`connected; engine ${gw.engine}; the engine's clock is ${gw.offset.toFixed(1)} ms from this machine's`);
  const driver = createDriver(gw);
  let ok = true;

  const names = driver.scenarios.map((s) => s.name);
  const first = [];
  for (const name of names) {
    const run = await driver.run(name);
    first.push(run);
    show(run);
    ok &&= run.pass;
  }
  const passed = first.filter((r) => r.pass).length;
  console.log(`${passed}/${first.length} scenarios passed`);

  const aggregate = (title, many) => {
    for (const run of many.runs) if (!run.pass) show(run);
    const p = many.perf;
    console.log(`${many.pass ? "PASS" : "FAIL"} ${title}: ${many.passed}/${many.runs.length} runs passed at concurrency ${many.concurrency}`);
    const l = p.latency_us;
    console.log(`  latency (gateway, command to batch)       p50 ${ms(l.p50)} ms   p95 ${ms(l.p95)} ms   max ${ms(l.max)} ms   (${l.n} commands)`);
    console.log(`  round trip (this client, send to result)  p50 ${msOf(p.rtt_ms.p50)} ms   p95 ${msOf(p.rtt_ms.p95)} ms   max ${msOf(p.rtt_ms.max)} ms`);
    console.log(`  ${p.commands} commands in ${secs(p.wall_ms)} s = ${p.commands_per_s.toFixed(1)} commands/s over the whole window (the scenarios' own waits included)`);
    ok &&= many.pass;
  };

  console.log(`\nhappy_path x${RUNS} at concurrency ${CONCURRENCY}`);
  aggregate(`happy_path x${RUNS}`, await driver.runMany(["happy_path"], RUNS, CONCURRENCY));
  if (MIX > 0) {
    console.log(`\nall ${names.length} scenarios x${MIX} at concurrency ${CONCURRENCY}`);
    aggregate(`all scenarios x${MIX}`, await driver.runMany(names, MIX, CONCURRENCY));
  }

  gw.close();
  console.log(ok ? "\nLIVE SMOKE OK" : "\nLIVE SMOKE FAILED");
  return ok;
}

main().then(
  (ok) => process.exit(ok ? 0 : 1),
  (e) => {
    console.error(`LIVE SMOKE FAILED: ${e instanceof Error ? e.stack : e}`);
    process.exit(1);
  },
);
