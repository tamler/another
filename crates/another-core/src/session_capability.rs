//! Unconnected session-capability cryptographic primitives.
//!
//! These functions do not authenticate a client, prove a binding, store or revoke a
//! capability durably, protect a transport, or authorize an edit. A future caller must
//! obtain the expected binding from a verified transaction and enforce the persisted
//! revocation state in that same transaction. No route uses this module.

use ring::hmac;

const ID_BYTES: usize = 16;
const SECRET_BYTES: usize = 32;
const TOKEN_BYTES: usize = ID_BYTES + SECRET_BYTES;
const MAX_IDENTITY_BYTES: usize = 256;
const MAX_LIFETIME_MS: i64 = 30 * 60 * 1000;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Binding {
    node_id: String,
    agent_id: String,
    project_id: String,
    client: String,
    session_digest: [u8; 32],
}

impl Binding {
    pub(crate) fn new(
        node_id: &str,
        agent_id: &str,
        project_id: &str,
        client: &str,
        session_digest: [u8; 32],
    ) -> Option<Self> {
        let valid = |value: &str| {
            !value.is_empty()
                && value.len() <= MAX_IDENTITY_BYTES
                && value.trim() == value
                && !value.chars().any(char::is_control)
        };
        if !valid(node_id)
            || !valid(agent_id)
            || !valid(project_id)
            || !matches!(client, "claude" | "codex" | "grok" | "cursor")
        {
            return None;
        }
        Some(Self {
            node_id: node_id.into(),
            agent_id: agent_id.into(),
            project_id: project_id.into(),
            client: client.into(),
            session_digest,
        })
    }
}

/// Bearer material must not be logged, serialized into events, or used as an actor name.
pub(crate) struct Capability([u8; TOKEN_BYTES]);

impl std::fmt::Debug for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Capability([redacted])")
    }
}

impl Capability {
    pub(crate) fn parse(bytes: &[u8]) -> Option<Self> {
        let bytes: [u8; TOKEN_BYTES] = bytes.try_into().ok()?;
        if bytes[..ID_BYTES].iter().all(|b| *b == 0) || bytes[ID_BYTES..].iter().all(|b| *b == 0) {
            return None;
        }
        Some(Self(bytes))
    }

    /// A transport must keep this material confidential; it is not a log value.
    pub(crate) fn expose_to_transport<T>(&self, use_bytes: impl FnOnce(&[u8]) -> T) -> T {
        use_bytes(&self.0)
    }
}

/// Persist only the verifier, never the bearer secret. Durable storage is not implemented.
pub(crate) struct Verifier {
    id: [u8; ID_BYTES],
    issued_ms: i64,
    expires_ms: i64,
    tag: [u8; 32],
    revoked: bool,
}

impl Verifier {
    /// Local state transition only; durable, atomic revocation is not implemented.
    pub(crate) fn revoke(&mut self) {
        self.revoked = true;
    }
}

fn signed_fields(
    binding: &Binding,
    id: &[u8; ID_BYTES],
    issued_ms: i64,
    expires_ms: i64,
) -> Vec<u8> {
    let mut bytes = b"another-session-capability-v1\0".to_vec();
    for field in [
        id.as_slice(),
        binding.node_id.as_bytes(),
        binding.agent_id.as_bytes(),
        binding.project_id.as_bytes(),
        binding.client.as_bytes(),
        binding.session_digest.as_slice(),
        issued_ms.to_be_bytes().as_slice(),
        expires_ms.to_be_bytes().as_slice(),
    ] {
        bytes.extend_from_slice(&(field.len() as u64).to_be_bytes());
        bytes.extend_from_slice(field);
    }
    bytes
}

pub(crate) fn mint(
    binding: &Binding,
    now_ms: i64,
    lifetime_ms: i64,
) -> Option<(Capability, Verifier)> {
    mint_with(binding, now_ms, lifetime_ms, |bytes| {
        getrandom::fill(bytes).is_ok()
    })
}

fn mint_with(
    binding: &Binding,
    now_ms: i64,
    lifetime_ms: i64,
    fill: impl FnOnce(&mut [u8]) -> bool,
) -> Option<(Capability, Verifier)> {
    if now_ms < 0 || !(1..=MAX_LIFETIME_MS).contains(&lifetime_ms) {
        return None;
    }
    let expires_ms = now_ms.checked_add(lifetime_ms)?;
    let mut bytes = [0; TOKEN_BYTES];
    if !fill(&mut bytes) {
        return None;
    }
    let capability = Capability::parse(&bytes)?;
    let mut id = [0; ID_BYTES];
    id.copy_from_slice(&bytes[..ID_BYTES]);
    let key = hmac::Key::new(hmac::HMAC_SHA256, &bytes[ID_BYTES..]);
    let mut tag = [0; 32];
    tag.copy_from_slice(
        hmac::sign(&key, &signed_fields(binding, &id, now_ms, expires_ms)).as_ref(),
    );
    Some((
        capability,
        Verifier {
            id,
            issued_ms: now_ms,
            expires_ms,
            tag,
            revoked: false,
        },
    ))
}

/// Verify only a currently trusted record against an independently verified binding.
/// Revocation/history and atomicity must be enforced by the eventual storage caller.
pub(crate) fn verify(
    capability: &Capability,
    record: &Verifier,
    expected: &Binding,
    now_ms: i64,
) -> bool {
    if record.revoked
        || now_ms < record.issued_ms
        || now_ms >= record.expires_ms
        || record
            .expires_ms
            .checked_sub(record.issued_ms)
            .is_none_or(|ttl| !(1..=MAX_LIFETIME_MS).contains(&ttl))
        || capability.0[..ID_BYTES] != record.id
    {
        return false;
    }
    let key = hmac::Key::new(hmac::HMAC_SHA256, &capability.0[ID_BYTES..]);
    hmac::verify(
        &key,
        &signed_fields(expected, &record.id, record.issued_ms, record.expires_ms),
        &record.tag,
    )
    .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(client: &str, digest: [u8; 32]) -> Binding {
        Binding::new("node", "agent", "project", client, digest).unwrap()
    }

    #[test]
    fn capability_is_bound_to_full_session_and_actor_and_revocable() {
        let original = binding("claude", [1; 32]);
        let (token, mut record) = mint(&original, 1_000, 30_000).unwrap();
        assert!(verify(&token, &record, &original, 1_001));
        // A session capability is reusable; operation-level replay must be handled
        // separately by the admission transaction.
        assert!(verify(&token, &record, &original, 1_002));
        assert!(!verify(&token, &record, &binding("codex", [1; 32]), 1_001));
        assert!(!verify(&token, &record, &binding("claude", [2; 32]), 1_001));
        assert!(!verify(
            &token,
            &record,
            &Binding::new("node", "other", "project", "claude", [1; 32]).unwrap(),
            1_001
        ));
        assert!(!verify(
            &token,
            &record,
            &Binding::new("other", "agent", "project", "claude", [1; 32]).unwrap(),
            1_001
        ));
        assert!(!verify(
            &token,
            &record,
            &Binding::new("node", "agent", "other", "claude", [1; 32]).unwrap(),
            1_001
        ));
        record.revoke();
        assert!(!verify(&token, &record, &original, 1_001));
    }

    #[test]
    fn malformed_expired_or_altered_capabilities_deny() {
        let original = binding("claude", [3; 32]);
        let (token, record) = mint(&original, 1_000, 10).unwrap();
        assert!(!verify(&token, &record, &original, 999));
        assert!(!verify(&token, &record, &original, 1_010));
        assert!(Capability::parse(&[]).is_none());
        assert!(Capability::parse(&[0; TOKEN_BYTES]).is_none());
        let mut altered = token.expose_to_transport(|bytes| bytes.to_vec());
        altered[TOKEN_BYTES - 1] ^= 1;
        assert!(!verify(
            &Capability::parse(&altered).unwrap(),
            &record,
            &original,
            1_001
        ));
        let (other, _) = mint(&original, 1_000, 10).unwrap();
        assert!(!verify(&other, &record, &original, 1_001));
        assert_eq!(format!("{token:?}"), "Capability([redacted])");
    }

    #[test]
    fn persisted_verifier_tampering_cannot_extend_or_reassign_a_capability() {
        let original = binding("claude", [5; 32]);
        let (token, mut record) = mint(&original, 1_000, 10).unwrap();
        record.expires_ms += 1;
        assert!(!verify(&token, &record, &original, 1_001));
        record.expires_ms -= 1;
        record.issued_ms -= 1;
        assert!(!verify(&token, &record, &original, 1_001));
        record.issued_ms += 1;
        record.id[0] ^= 1;
        assert!(!verify(&token, &record, &original, 1_001));
        record.id[0] ^= 1;
        record.tag[0] ^= 1;
        assert!(!verify(&token, &record, &original, 1_001));
    }

    #[test]
    fn malformed_bindings_bad_lifetimes_and_entropy_failure_deny() {
        assert!(Binding::new("", "agent", "project", "claude", [0; 32]).is_none());
        assert!(Binding::new("node", "agent", "project", "unknown", [0; 32]).is_none());
        assert!(Binding::new("node\n", "agent", "project", "claude", [0; 32]).is_none());
        let original = binding("claude", [4; 32]);
        assert!(mint(&original, -1, 1).is_none());
        assert!(mint(&original, 1, 0).is_none());
        assert!(mint(&original, 1, MAX_LIFETIME_MS + 1).is_none());
        assert!(mint(&original, i64::MAX, 1).is_none());
        assert!(mint_with(&original, 1, 1, |_| false).is_none());
        assert!(mint_with(&original, 1, 1, |_| true).is_none());
    }
}
