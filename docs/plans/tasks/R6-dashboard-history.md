# Task R6: The dashboard loads its history on page load

**Spec:** [read-api.md](../../specs/read-api.md) §8. **Goal:** a reloaded dashboard shows the same lanes, contracts, positions, ledger and trace as before the reload.

**Files:**
- Modify:
  - `tools/dashboard/web/recorder.js` (`seedHistory`);
  - `tools/dashboard/web/model.js` (`commandOf` falls back to a batch's own `command`);
  - `tools/dashboard/web/index.html` (`HTTP_BASE`, `loadHistory()`, a banner on failure);
  - `tools/dashboard/web/recorder.test.mjs`, `model.test.mjs`, `page.test.mjs`;
  - `tools/dashboard/DESIGN.md` §11 (one paragraph: history on load).

**Interfaces.**
- Consumes (R4): `GET /v1/status` → `{replica: {last_seq}}`; `GET /v1/trace?view=admin&after_seq=N&limit=1000` → `{entries: [{seq, time, kind, origin, events, balances, command}], next_after_seq}`.
- Produces:

```js
// recorder.js, on the object createRecorder() returns
/** Adds history batches (ascending by seq) to the current epoch: only seqs below the first live
 *  batch of the epoch that are not present yet. They come first in allBatches(), each marked
 *  `history: true` and carrying its `command`. Returns how many were added. */
seedHistory(entries) -> number
```

## Requirements

1. **`HTTP_BASE`:** `?http=` on the page URL if it is an `http(s)://` URL, else `http://127.0.0.1:18081` (an invalid `?http=` is ignored with a banner, as `?gateway=` is).
2. **`loadHistory()`** runs once at page load, without delaying the WebSocket connection:
   1. `fetch(HTTP_BASE + "/v1/status")`; `from = max(0, replica.last_seq - 5000)`;
   2. page through `/v1/trace?view=admin&after_seq=…&limit=1000` until `next_after_seq` is `null`;
   3. `rec.seedHistory(entries)`, then render.

   On any failure (network, non-200, bad JSON): no history, a dismissible banner `History not loaded (<reason>): showing what happens from now on.`, and the page works as today.
3. **`seedHistory` and live batches:**
   - a live batch that arrived during the fetch stays where it is; history only fills seqs below the first live one;
   - a live frame whose `seq` is already present as history is ignored (never a new epoch, never a duplicate);
   - history never sets the recorder's "newest" batch, so chunk merging and epoch detection of live frames are unchanged.
4. **Model:** where the page has no command of its own for a batch (`commandAt` returns nothing), use the batch's `command`, under the same rule as today (it must match the batch's `origin` actor and nonce). That restores limits and quote sizes after a reload.
5. **Unchanged:** the runs table (the page's own runs), the account views (their second connection starts when the view is chosen, as today), the timeline (history batches are batches like any other, so the scrubber covers them).

## Steps

- [ ] **Step 1: Failing recorder tests** (`recorder.test.mjs`): seeding into an empty recorder (order, `history: true`, `command` kept); seeding with live batches already present (only lower seqs added, live order kept after history); a live frame duplicating a history seq is ignored and does not start an epoch; a live chunk after seeding still merges with its live batch; seeding twice adds nothing the second time.
- [ ] **Step 2:** Implement `seedHistory` and the duplicate rule in `mergeBatch`. Run: PASS.
- [ ] **Step 3: Failing model test** (`model.test.mjs`): a history batch with `command` (a `SubmitRequest` with limit 7000) and no `commandAt` entry → the model's request shows limit 0.70; a `command` whose actor or nonce does not match the origin is ignored.
- [ ] **Step 4:** Implement the fallback. Run: PASS.
- [ ] **Step 5: Failing page tests** (`page.test.mjs`, with a stubbed `fetch` in the page's VM context): the page loads a status and two trace pages, then the trace panel lists the history batches and the ledger shows their balances; a `fetch` that rejects → the banner text and the page still connects; `?http=ftp://x` → ignored with a banner.
- [ ] **Step 6:** Implement `HTTP_BASE`, `loadHistory()` and the banner. Run `tools/dashboard/build.sh`: every suite passes.
- [ ] **Step 7: Live check:** with the stack from R5 up, run a scenario in the built page, reload it, and confirm (headless Chrome screenshot, or the live smoke's harness) that the trace and the ledger show the earlier run's batches. `node tools/dashboard/web/live-smoke.mjs` → 14/14.
- [ ] **Step 8: Commit:** `feat(dashboard): load history from the read API on page load`.
