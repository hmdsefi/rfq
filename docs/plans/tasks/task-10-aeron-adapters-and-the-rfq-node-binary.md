# Task 10: Aeron adapters and the `rfq-node` binary

> Part of the [implementation roadmap](../2026-10-13-rfq-engine-plan.md). Its **Global Constraints** and **Layered structure** table apply to this task and supersede crate names/paths written here.

**Files:**
- Create: `crates/rfq-node/src/aeron.rs`, `crates/rfq-node/src/main.rs`
- Modify: `crates/rfq-node/Cargo.toml`, `crates/rfq-node/src/lib.rs`

**Interfaces:**
- Consumes: the Task 0 "Verified API" section (this is the source of truth for every `rusteron` call below), plus Task 9 traits.
- Produces: `aeron::{connect(dir) -> anyhow::Result<Aeron>, AeronTransport::new(&Aeron) -> anyhow::Result<AeronTransport>, ArchiveJournal::new(&Aeron) -> anyhow::Result<ArchiveJournal>}`. Constants `INGRESS = 1001, LOG = 1002, EGRESS = 1003, REPLAY = 1004`. The binary is configured by env vars `AERON_DIR` (default `/dev/shm/aeron`), `RFQ_JOURNAL` (`archive` | `file:<path>`), and `RFQ_HASH_EVERY_BATCH` (`1` to log the state hash after each batch).

- [ ] **Step 1: Add dependencies**

In `crates/rfq-node/Cargo.toml`:
```toml
rusteron-archive = { version = "=<version from Task 0>", features = ["static", "precompile"] }
tracing-subscriber = { workspace = true }
```
Add `pub mod aeron;` to `lib.rs`.

- [ ] **Step 2: Implement the adapters**

`crates/rfq-node/src/aeron.rs` (adjust each `rusteron` call to the Verified API from Task 0; the structure and semantics must stay as written):
```rust
use std::time::Duration;

use anyhow::{anyhow, bail};
use rfq_core::codec::{decode, encode};
use rfq_core::SequencedCommand;
use rusteron_archive::*;

use crate::journal::Journal;
use crate::transport::Transport;

pub const INGRESS: i32 = 1001;
pub const LOG: i32 = 1002;
pub const EGRESS: i32 = 1003;
pub const REPLAY: i32 = 1004;
const TIMEOUT: Duration = Duration::from_secs(10);

fn ipc() -> std::ffi::CString { "aeron:ipc".into_c_string() }

pub fn connect(dir: &str) -> anyhow::Result<Aeron> {
    let ctx = AeronContext::new()?;
    ctx.set_dir(&dir.into_c_string())?;
    let aeron = Aeron::new(&ctx)?;
    aeron.start()?;
    Ok(aeron)
}

pub struct AeronTransport {
    ingress: AeronSubscription,
    egress: AeronPublication,
}

impl AeronTransport {
    pub fn new(aeron: &Aeron) -> anyhow::Result<Self> {
        let ingress = aeron
            .async_add_subscription(&ipc(), INGRESS, Handlers::no_available_image_handler(), Handlers::no_unavailable_image_handler())?
            .poll_blocking(TIMEOUT)?;
        let egress = aeron.async_add_publication(&ipc(), EGRESS)?.poll_blocking(TIMEOUT)?;
        Ok(AeronTransport { ingress, egress })
    }
}

impl Transport for AeronTransport {
    fn poll_ingress(&mut self, max: usize, sink: &mut dyn FnMut(&[u8])) {
        let _ = self.ingress.poll_once(|msg: &[u8], _h: AeronHeader| sink(msg), max);
    }

    /// Events are derivable from the log, so with no subscriber they are dropped, not blocked on.
    fn publish_egress(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        for _ in 0..1_000 {
            let r = self.egress.offer(bytes, Handlers::no_reserved_value_supplier_handler());
            if r > 0 {
                return Ok(());
            }
            if r == AERON_PUBLICATION_NOT_CONNECTED as i64 {
                return Ok(());
            }
            if r == AERON_PUBLICATION_CLOSED as i64 {
                bail!("egress publication closed");
            }
            std::thread::yield_now(); // back pressure / admin action
        }
        bail!("egress back-pressured for too long")
    }
}

/// The log is the set of Archive recordings of stream 1002, in recording-id order.
pub struct ArchiveJournal {
    aeron: Aeron,
    archive: AeronArchive,
    publication: AeronPublication,
    recording_id: Option<i64>,
}

impl ArchiveJournal {
    pub fn new(aeron: &Aeron) -> anyhow::Result<Self> {
        let ctx = AeronArchiveContext::new_with_no_credentials_supplier(
            aeron,
            "aeron:udp?endpoint=localhost:8010".into_c_string().as_c_str(),
            "aeron:udp?endpoint=localhost:0".into_c_string().as_c_str(),
            "aeron:udp?endpoint=localhost:8030".into_c_string().as_c_str(),
        )?;
        let archive = AeronArchiveAsyncConnect::new_with_aeron(&ctx, aeron)?.poll_blocking(TIMEOUT)?;
        let publication = aeron.async_add_publication(&ipc(), LOG)?.poll_blocking(TIMEOUT)?;
        Ok(ArchiveJournal { aeron: aeron.clone(), archive, publication, recording_id: None })
    }

    fn recordings(&self) -> anyhow::Result<Vec<i64>> {
        let mut ids = Vec::new();
        self.archive.list_recordings_for_uri(0, i32::MAX, &ipc(), LOG, |d: AeronArchiveRecordingDescriptor| ids.push(d.recording_id()))?;
        ids.sort_unstable();
        Ok(ids)
    }
}

impl Journal for ArchiveJournal {
    fn append(&mut self, sc: &SequencedCommand) -> anyhow::Result<u64> {
        if self.recording_id.is_none() {
            let before = self.recordings()?;
            self.archive.start_recording(&ipc(), LOG, SOURCE_LOCATION_LOCAL, true)?;
            let id = loop {
                let now = self.recordings()?;
                if let Some(id) = now.into_iter().find(|id| !before.contains(id)) {
                    break id;
                }
                std::thread::sleep(Duration::from_millis(1));
            };
            self.recording_id = Some(id);
        }
        let bytes = encode(sc);
        loop {
            let r = self.publication.offer(&bytes, Handlers::no_reserved_value_supplier_handler());
            if r > 0 {
                return Ok(r as u64);
            }
            if r == AERON_PUBLICATION_CLOSED as i64 {
                bail!("log publication closed");
            }
            std::thread::yield_now();
        }
    }

    fn wait_durable(&mut self, pos: u64) -> anyhow::Result<()> {
        let id = self.recording_id.ok_or_else(|| anyhow!("no active recording"))?;
        while (self.archive.get_recording_position(id)? as u64) < pos {
            std::thread::yield_now();
        }
        Ok(())
    }

    fn replay(&mut self, f: &mut dyn FnMut(SequencedCommand)) -> anyhow::Result<()> {
        for id in self.recordings()? {
            let stop = self.archive.get_stop_position(id)?;
            if stop <= 0 {
                continue; // empty recording
            }
            let sub = self
                .aeron
                .async_add_subscription(&ipc(), REPLAY, Handlers::no_available_image_handler(), Handlers::no_unavailable_image_handler())?
                .poll_blocking(TIMEOUT)?;
            let params = AeronArchiveReplayParams::new(0, i32::MAX, 0, stop, 0, 0)?;
            self.archive.start_replay(id, &ipc(), REPLAY, &params)?;
            let mut consumed = 0i64;
            while consumed < stop {
                sub.poll_once(
                    |msg: &[u8], h: AeronHeader| {
                        consumed = h.position();
                        match decode::<SequencedCommand>(msg) {
                            Ok(sc) => f(sc),
                            Err(e) => tracing::error!(?e, "corrupt log record"),
                        }
                    },
                    256,
                )?;
            }
        }
        Ok(())
    }
}
```
**Note:** if `get_stop_position` returns "still active" (`-1`) for the recording of a crashed node, use `get_recording_position(id)` as the stop position instead. The spike shows which applies.

- [ ] **Step 3: Write the binary**

`crates/rfq-node/src/main.rs`:
```rust
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rfq_core::scenarios::demo_config;
use rfq_core::testkit::genesis;
use rfq_node::aeron::{connect, ArchiveJournal, AeronTransport};
use rfq_node::journal::{FileJournal, Journal};
use rfq_node::node::{hex, Node};

fn now_ms() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64 }

fn run<J: Journal>(journal: J, transport: AeronTransport) -> anyhow::Result<()> {
    let hash_every_batch = std::env::var("RFQ_HASH_EVERY_BATCH").as_deref() == Ok("1");
    let mut node = Node::start(journal, transport, genesis(demo_config()))?;
    loop {
        let n = node.poll(now_ms())?;
        if n == 0 {
            std::thread::sleep(Duration::from_millis(1));
        } else if hash_every_batch {
            tracing::info!(seq = node.engine().last_seq(), hash = %hex(&node.state_hash()), "applied");
        }
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_target(false).init();
    let dir = std::env::var("AERON_DIR").unwrap_or_else(|_| "/dev/shm/aeron".into());
    let aeron = connect(&dir)?;
    let transport = AeronTransport::new(&aeron)?;
    match std::env::var("RFQ_JOURNAL").unwrap_or_else(|_| "archive".into()).as_str() {
        "archive" => run(ArchiveJournal::new(&aeron)?, transport),
        other => {
            let path = other.strip_prefix("file:").ok_or_else(|| anyhow::anyhow!("RFQ_JOURNAL must be archive or file:<path>"))?;
            run(FileJournal::open(path.as_ref())?, transport)
        }
    }
}
```
The demo genesis uses `demo_config()` so the scripted scenarios run in seconds. The default `Config` is what the tests use.

- [ ] **Step 4: Build**

Run: `cargo build -p rfq-node`
Expected: compiles. On macOS the `precompile` feature should link without cmake; if it doesn't, build inside Docker only (Task 11).

- [ ] **Step 5: Commit**

```bash
git add crates/rfq-node
git commit -m "feat(node): Aeron transport and Archive-backed journal; rfq-node binary"
```

