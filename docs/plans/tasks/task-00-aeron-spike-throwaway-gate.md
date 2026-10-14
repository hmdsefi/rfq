# Task 0: Aeron spike (throwaway, gate)

> Part of the [implementation roadmap](../2026-10-13-rfq-engine-plan.md). Its **Global Constraints** and **Layered structure** table apply to this task and supersede crate names/paths written here.

**Goal:** Prove these four things before committing to Aeron: `rusteron` builds in a Linux container on Apple Silicon; a Rust client and the Java `ArchivingMediaDriver` share `/dev/shm` across containers; Archive records an IPC stream; and the Rust archive client can replay it. **Output:** the verified API calls and pinned versions, written into `docs/specs/transport-aeron.md` → "Verified API". If any of the four fails after 60 minutes, stop and switch `ArchiveJournal` to `FileJournal` (Task 9) as the durable journal. Record that decision in the RFC risks table.

**Files:**
- Create: `spike/Cargo.toml`, `spike/src/main.rs`, `spike/docker-compose.yml`, `spike/Dockerfile`, `docker/aeron/Dockerfile`
- Modify: `docs/specs/transport-aeron.md` (append "Verified API")

- [ ] **Step 1: Start Docker Desktop and create the lane-B worktree**

```bash
open -a Docker
cd ~/projects/rfq && git worktree add ../rfq-spike -b task-0-spike main && cd ../rfq-spike
docker info --format '{{.OSType}}/{{.Architecture}}'
```
Run every remaining Task 0 step from `../rfq-spike`.
Expected: `linux/aarch64`

- [ ] **Step 2: Find the Aeron version bundled by rusteron**

```bash
cargo search rusteron-archive --limit 1
cargo info rusteron-archive
```
Note the crate version (e.g. `0.2.11`). In its repository, the `aeron` git submodule tag gives the Aeron C version (e.g. `1.48.x`). The Java jar must use the same `1.x` minor version.

- [ ] **Step 3: Write the Java archive image**

`docker/aeron/Dockerfile`:
```dockerfile
FROM eclipse-temurin:21-jre
ARG AERON_VERSION=1.48.5
ADD https://repo1.maven.org/maven2/io/aeron/aeron-all/${AERON_VERSION}/aeron-all-${AERON_VERSION}.jar /opt/aeron/aeron-all.jar
ENTRYPOINT ["java", "--add-opens", "java.base/jdk.internal.misc=ALL-UNNAMED", "--add-opens", "java.base/sun.nio.ch=ALL-UNNAMED", \
  "-Daeron.dir=/dev/shm/aeron", "-Daeron.dir.delete.on.start=true", \
  "-Daeron.archive.dir=/data/archive", \
  "-Daeron.archive.control.channel=aeron:udp?endpoint=localhost:8010", \
  "-Daeron.archive.replication.channel=aeron:udp?endpoint=localhost:0", \
  "-Daeron.archive.recording.events.enabled=false", \
  "-cp", "/opt/aeron/aeron-all.jar", "io.aeron.archive.ArchivingMediaDriver"]
```
Set `AERON_VERSION` to the version found in Step 2.

- [ ] **Step 4: Write the spike client**

`spike/Cargo.toml`:
```toml
[package]
name = "spike"
version = "0.0.0"
edition = "2021"

[dependencies]
rusteron-archive = { version = "=0.2.11", features = ["static", "precompile"] }
```

`spike/src/main.rs`:
```rust
use rusteron_archive::*;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ctx = AeronContext::new()?;
    ctx.set_dir(&"/dev/shm/aeron".into_c_string())?;
    let aeron = Aeron::new(&ctx)?;
    aeron.start()?;

    let channel = "aeron:ipc".into_c_string();
    let archive_ctx = AeronArchiveContext::new_with_no_credentials_supplier(
        &aeron,
        "aeron:ipc".into_c_string().as_c_str(),            // control request (local)
        "aeron:ipc".into_c_string().as_c_str(),            // control response
        "aeron:udp?endpoint=localhost:8011".into_c_string().as_c_str(), // recording events (unused)
    )?;
    let archive = AeronArchiveAsyncConnect::new_with_aeron(&archive_ctx, &aeron)?
        .poll_blocking(Duration::from_secs(10))?;

    archive.start_recording(&channel, 1002, SOURCE_LOCATION_LOCAL, true)?;
    let publication = aeron
        .async_add_publication(&channel, 1002)?
        .poll_blocking(Duration::from_secs(5))?;
    let mut last = 0;
    for i in 0..3u8 {
        loop {
            let r = publication.offer(&[i; 16], Handlers::no_reserved_value_supplier_handler());
            if r > 0 { last = r; break; }
        }
    }
    println!("published up to position {last}");

    let mut found = Vec::new();
    archive.list_recordings_for_uri(0, i32::MAX, &channel, 1002, |d: AeronArchiveRecordingDescriptor| {
        found.push(d.recording_id());
    })?;
    let recording_id = *found.last().expect("a recording exists");
    while archive.get_recording_position(recording_id)? < last {
        std::thread::sleep(Duration::from_millis(1));
    }
    println!("recording {recording_id} durable up to {last}");

    let replay_sub = aeron
        .async_add_subscription(&channel, 1004, Handlers::no_available_image_handler(), Handlers::no_unavailable_image_handler())?
        .poll_blocking(Duration::from_secs(5))?;
    let params = AeronArchiveReplayParams::new(0, i32::MAX, 0, last, 0, 0)?;
    archive.start_replay(recording_id, &channel, 1004, &params)?;
    let mut got = 0;
    while got < 3 {
        got += replay_sub.poll_once(|msg: &[u8], _h: AeronHeader| println!("replayed {:?}", &msg[..2]), 10)?;
    }
    println!("SPIKE OK");
    Ok(())
}
```
These calls are a best guess at the `rusteron` API. Fix any compile errors against `docs.rs/rusteron-archive` and its `examples/record_and_replay.rs`. **Whatever compiles and runs here is the API that Task 10 uses.**

- [ ] **Step 5: Compose and run**

`spike/Dockerfile`:
```dockerfile
FROM rust:1-bookworm
WORKDIR /spike
COPY . .
RUN cargo build --release
CMD ["/spike/target/release/spike"]
```
`spike/docker-compose.yml`:
```yaml
services:
  aeron:
    build: ../docker/aeron
    ipc: shareable
    shm_size: 512m
  spike:
    build: .
    ipc: "service:aeron"
    depends_on: [aeron]
```
```bash
cd spike && docker compose up --build --abort-on-container-exit spike
```
Expected: the output ends with `SPIKE OK`. If `precompile` fails to download prebuilt libraries for aarch64 Linux, add `RUN apt-get update && apt-get install -y cmake clang default-jdk-headless` to `spike/Dockerfile`, drop the `precompile` feature, and retry.

- [ ] **Step 6: Record the verified API and versions**

Append a section to `docs/specs/transport-aeron.md`:
```markdown
## Verified API (Task 0)
- rusteron-archive = <version>, Aeron C <version>; aeron-all jar <version>
- Context/connect: <exact calls that worked>
- Publish: <call + return type>; Subscribe/poll: <call + closure signature>
- Recording: start_recording(...), list_recordings_for_uri(...), get_recording_position(...)
- Replay: <params constructor + start_replay call>
- Build: features <...>; extra apt packages: <none | list>
```

- [ ] **Step 7: Delete the spike, commit the findings, merge lane B**

```bash
rm -rf spike
git add docs/specs/transport-aeron.md docker/aeron/Dockerfile
git commit -m "spike: pin Aeron versions and record verified rusteron API"
cd ~/projects/rfq && git merge --no-ff task-0-spike && git worktree remove ../rfq-spike
```

