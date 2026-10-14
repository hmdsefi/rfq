//! Wire format: one version byte, then bincode (fixint, little-endian, trailing bytes rejected).
//!
//! Adapter-layer concern. The application layer only sees typed values (`ClientCommand`,
//! `EventBatch`); this module is where bytes become those values and back.
//!
//! Size policy: limits apply only at the untrusted ingress boundary ([`decode_ingress`] enforces
//! [`MAX_INGRESS`]). Data the node itself produces (journal records, egress batches) is never
//! size-limited by the codec, so [`encode`] cannot fail, and the trusted [`decode`] has no limit.
//! (bincode 1.3's `Options::deserialize` over a slice ignores `with_limit`, so a limit configured
//! there would not be enforced anyway; the explicit length check is the honest mechanism.)
use bincode::Options;
use rfq_app::ClientCommand;
use serde::{Serialize, de::DeserializeOwned};

pub const VERSION: u8 = 1;

/// Largest ingress message (version byte included) the node will even try to decode.
pub const MAX_INGRESS: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CodecError {
    #[error("empty message")]
    Empty,
    #[error("unsupported codec version {0}")]
    UnsupportedVersion(u8),
    #[error("malformed message: {0}")]
    Malformed(String),
}

/// Shared by `encode` and `decode`. `DefaultOptions` rejects trailing bytes on decode.
fn opts() -> impl Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
}

/// Infallible: serializing an in-memory value with no size limit cannot fail (the domain types
/// have no custom `Serialize` impls and no unsized sequences).
pub fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    let mut bytes = vec![VERSION];
    bytes.extend(
        opts()
            .serialize(value)
            .expect("in-memory serialization cannot fail"),
    );
    bytes
}

/// `encode(value).len()`, computed without allocating.
pub fn encoded_len<T: Serialize>(value: &T) -> usize {
    let body = opts()
        .serialized_size(value)
        .expect("in-memory serialization cannot fail");
    1 + usize::try_from(body).expect("an in-memory value's encoding fits in memory")
}

/// Trusted decoder (journal replay, egress): no size limit. Untrusted input must go through
/// [`decode_ingress`].
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CodecError> {
    match bytes.split_first() {
        None => Err(CodecError::Empty),
        Some((&VERSION, rest)) => opts()
            .deserialize(rest)
            .map_err(|e| CodecError::Malformed(e.to_string())),
        Some((v, _)) => Err(CodecError::UnsupportedVersion(*v)),
    }
}

/// Decode one ingress message. Oversized or malformed input is logged and dropped: a hostile or
/// buggy client must never be able to stop the node.
pub fn decode_ingress(bytes: &[u8]) -> Option<ClientCommand> {
    if bytes.len() > MAX_INGRESS {
        tracing::warn!(
            len = bytes.len(),
            max = MAX_INGRESS,
            "dropping oversized ingress message"
        );
        return None;
    }
    match decode(bytes) {
        Ok(cmd) => Some(cmd),
        Err(error) => {
            tracing::warn!(%error, len = bytes.len(), "dropping malformed ingress message");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rfq_domain::{AccountId, Action, Command, Config, ContractId, Genesis};

    const ALICE: AccountId = AccountId(10);

    #[test]
    fn roundtrip_client_command() {
        let c = ClientCommand {
            actor: ALICE,
            nonce: 3,
            action: Action::Withdraw { amount: 5 },
        };
        assert_eq!(decode::<ClientCommand>(&encode(&c)).unwrap(), c);
    }

    #[test]
    fn rejects_empty_and_wrong_version() {
        assert_eq!(decode::<ClientCommand>(&[]), Err(CodecError::Empty));
        let mut bytes = encode(&ClientCommand {
            actor: ALICE,
            nonce: 1,
            action: Action::Withdraw { amount: 1 },
        });
        bytes[0] = 9;
        assert_eq!(
            decode::<ClientCommand>(&bytes),
            Err(CodecError::UnsupportedVersion(9))
        );
    }

    /// A `ClientCommand` whose encoding is far above `MAX_INGRESS`.
    fn big_command() -> ClientCommand {
        ClientCommand {
            actor: ALICE,
            nonce: 1,
            action: Action::ListContract {
                id: ContractId(1),
                description: "x".repeat(100_000),
                oracle: AccountId(4),
                trading_close: 1,
                resolution_deadline: 2,
                dispute_window: 3,
            },
        }
    }

    #[test]
    fn forged_length_prefix_does_not_allocate() {
        // A Vec<LegSpec> length prefix claiming 2^40 elements must not allocate.
        let mut bytes = vec![VERSION];
        bytes.extend(10u64.to_le_bytes()); // actor
        bytes.extend(1u64.to_le_bytes()); // nonce
        bytes.extend(3u32.to_le_bytes()); // Action::SubmitRequest variant
        bytes.extend((1u64 << 40).to_le_bytes()); // legs length
        assert!(matches!(
            decode::<ClientCommand>(&bytes),
            Err(CodecError::Malformed(_))
        ));
    }

    #[test]
    fn oversized_ingress_is_dropped() {
        let cmd = big_command();
        let bytes = encode(&cmd);
        assert!(bytes.len() > MAX_INGRESS);
        assert!(decode_ingress(&bytes).is_none());
        // The trusted decoder (journal, egress) has no size limit and reads the same bytes fine.
        assert_eq!(decode::<ClientCommand>(&bytes).unwrap(), cmd);
    }

    #[test]
    fn encoded_len_matches_encode() {
        let cmd = big_command();
        assert_eq!(encoded_len(&cmd), encode(&cmd).len());
        assert_eq!(encoded_len(&Command::Tick), encode(&Command::Tick).len());
    }

    #[test]
    fn encode_large_value_does_not_panic() {
        let cmd = big_command();
        assert_eq!(decode::<ClientCommand>(&encode(&cmd)).unwrap(), cmd);
    }

    #[test]
    fn system_command_bytes_do_not_decode_as_client_command() {
        assert!(decode::<ClientCommand>(&encode(&Command::Tick)).is_err());
    }

    #[test]
    fn malformed_ingress_is_dropped() {
        assert!(decode_ingress(&[1, 2, 3]).is_none());
        assert!(decode_ingress(&[]).is_none());
        let valid = ClientCommand {
            actor: ALICE,
            nonce: 7,
            action: Action::Withdraw { amount: 1 },
        };
        assert_eq!(decode_ingress(&encode(&valid)), Some(valid));
    }

    #[test]
    fn client_cannot_inject_system_commands() {
        let genesis = Genesis {
            operator: AccountId(1),
            arbiter: AccountId(2),
            treasury: AccountId(3),
            config: Config::default(),
        };
        for system in [Command::Tick, Command::Resume, Command::Genesis(genesis)] {
            assert!(
                decode_ingress(&encode(&system)).is_none(),
                "system command {system:?} crossed ingress"
            );
        }
    }
}
