//! Wire format: one version byte, then bincode (fixint, little-endian, size-limited).
//!
//! Adapter-layer concern. The application layer only sees typed values (`ClientCommand`,
//! `EventBatch`); this module is where bytes become those values and back.
use bincode::Options;
use rfq_app::ClientCommand;
use serde::{Serialize, de::DeserializeOwned};

pub const VERSION: u8 = 1;
pub const MAX_MESSAGE: u64 = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CodecError {
    #[error("empty message")]
    Empty,
    #[error("unsupported codec version {0}")]
    UnsupportedVersion(u8),
    #[error("malformed message: {0}")]
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
        Some((&VERSION, rest)) => opts()
            .deserialize(rest)
            .map_err(|e| CodecError::Malformed(e.to_string())),
        Some((v, _)) => Err(CodecError::UnsupportedVersion(*v)),
    }
}

/// Decode one ingress message. Anything that is not a well-formed `ClientCommand` is logged and
/// dropped: a hostile or buggy client must never be able to stop the node.
pub fn decode_ingress(bytes: &[u8]) -> Option<ClientCommand> {
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
    use rfq_domain::{AccountId, Action, Command, Config, Genesis};

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

    #[test]
    fn decode_rejects_oversized() {
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
            let decoded = decode_ingress(&encode(&system));
            assert!(
                decoded.is_none() || matches!(decoded.unwrap().into_command(), Command::User { .. }),
                "system command {system:?} crossed ingress as a non-User command"
            );
        }
    }
}
