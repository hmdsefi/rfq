# Task 8: Codec

> Part of the [implementation roadmap](../2026-10-13-rfq-engine-plan.md). Its **Global Constraints** and **Layered structure** table apply to this task and supersede crate names/paths written here.

**Files:**
- Create: `crates/rfq-core/src/codec.rs`
- Modify: `crates/rfq-core/src/lib.rs` (add `pub mod codec;`)

**Interfaces:**
- Produces: `codec::{VERSION, MAX_MESSAGE, encode<T: Serialize>(&T) -> Vec<u8>, decode<T: DeserializeOwned>(&[u8]) -> Result<T, CodecError>, CodecError::{Empty, UnsupportedVersion(u8), Malformed(String)}, ClientCommand { actor, nonce, action }, ClientCommand::into_command(self) -> Command, EventBatch { seq, origin: Option<(AccountId, u64)>, events: Vec<Emitted> }}`.

- [ ] **Step 1: Write the failing tests**

At the bottom of `crates/rfq-core/src/codec.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::accounts::ALICE;

    #[test]
    fn roundtrip_client_command() {
        let c = ClientCommand { actor: ALICE, nonce: 3, action: Action::Withdraw { amount: 5 } };
        assert_eq!(decode::<ClientCommand>(&encode(&c)).unwrap(), c);
    }

    #[test]
    fn rejects_empty_and_wrong_version() {
        assert_eq!(decode::<ClientCommand>(&[]), Err(CodecError::Empty));
        let mut bytes = encode(&ClientCommand { actor: ALICE, nonce: 1, action: Action::Withdraw { amount: 1 } });
        bytes[0] = 9;
        assert_eq!(decode::<ClientCommand>(&bytes), Err(CodecError::UnsupportedVersion(9)));
    }

    #[test]
    fn decode_rejects_oversized() {
        // A Vec<LegSpec> length prefix claiming 2^40 elements must not allocate.
        let mut bytes = vec![VERSION];
        bytes.extend(10u64.to_le_bytes()); // actor
        bytes.extend(1u64.to_le_bytes()); // nonce
        bytes.extend(3u32.to_le_bytes()); // Action::SubmitRequest variant
        bytes.extend((1u64 << 40).to_le_bytes()); // legs length
        assert!(matches!(decode::<ClientCommand>(&bytes), Err(CodecError::Malformed(_))));
    }

    #[test]
    fn system_command_bytes_do_not_decode_as_client_command() {
        assert!(decode::<ClientCommand>(&encode(&Command::Tick)).is_err());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Add `pub mod codec;` to `lib.rs`.
Run: `cargo test -p rfq-core codec`
Expected: FAIL to compile.

- [ ] **Step 3: Implement the codec**

Prepend to `crates/rfq-core/src/codec.rs`:
```rust
//! Wire format: one version byte, then bincode (fixint, little-endian, size-limited).
use bincode::Options;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::command::{Action, Command};
use crate::event::Emitted;
use crate::types::AccountId;

pub const VERSION: u8 = 1;
pub const MAX_MESSAGE: u64 = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodecError {
    Empty,
    UnsupportedVersion(u8),
    Malformed(String),
}

fn opts() -> impl Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .with_limit(MAX_MESSAGE)
}

pub fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    let mut bytes = vec![VERSION];
    bytes.extend(opts().serialize(value).expect("message fits MAX_MESSAGE"));
    bytes
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CodecError> {
    match bytes.split_first() {
        None => Err(CodecError::Empty),
        Some((&VERSION, rest)) => opts().deserialize(rest).map_err(|e| CodecError::Malformed(e.to_string())),
        Some((v, _)) => Err(CodecError::UnsupportedVersion(*v)),
    }
}

/// What a client may put on ingress. It cannot express `Genesis` or `Tick`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientCommand {
    pub actor: AccountId,
    pub nonce: u64,
    pub action: Action,
}

impl ClientCommand {
    pub fn into_command(self) -> Command {
        Command::User { actor: self.actor, nonce: self.nonce, action: self.action }
    }
}

/// One egress message per applied command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventBatch {
    pub seq: u64,
    /// `(actor, nonce)` of the user command that produced these events; `None` for ticks.
    pub origin: Option<(AccountId, u64)>,
    pub events: Vec<Emitted>,
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p rfq-core codec`
Expected: 4 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/rfq-core
git commit -m "feat(core): versioned size-limited codec, ClientCommand and EventBatch"
```

