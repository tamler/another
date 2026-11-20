//! Node keys and signatures. Every node has an Ed25519 key pair; each event it authors is
//! signed, so events relayed through a hub can be checked against their origin node.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn unb64(text: &str) -> Result<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(text.trim())
        .map_err(|e| Error::Invalid(format!("invalid base64: {e}")))
}

pub fn generate_key() -> Result<SigningKey> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed)
        .map_err(|e| Error::Invalid(format!("no system randomness available: {e}")))?;
    Ok(SigningKey::from_bytes(&seed))
}

pub fn decode_secret(text: &str) -> Result<SigningKey> {
    let bytes: [u8; 32] = unb64(text)?
        .try_into()
        .map_err(|_| Error::Invalid("node secret key must be 32 bytes".into()))?;
    Ok(SigningKey::from_bytes(&bytes))
}

pub fn decode_public(text: &str) -> Result<VerifyingKey> {
    let bytes: [u8; 32] = unb64(text)?
        .try_into()
        .map_err(|_| Error::Invalid("node public key must be 32 bytes".into()))?;
    VerifyingKey::from_bytes(&bytes).map_err(|e| Error::Invalid(format!("invalid public key: {e}")))
}

/// [`decode_public`] for another node's key, before it is pinned. A small-order ("weak")
/// key is refused: under one, the degenerate signature verifies for every message, so
/// anyone could sign as that node (FINDINGS-main-weak-keys).
pub fn decode_peer_public(text: &str) -> Result<VerifyingKey> {
    let key = decode_public(text)?;
    if key.is_weak() {
        return Err(Error::Invalid(
            "node public key is a weak (small-order) Ed25519 key".into(),
        ));
    }
    Ok(key)
}

pub fn public_of(key: &SigningKey) -> String {
    b64(key.verifying_key().as_bytes())
}

/// Short, human-comparable fingerprint of a public key.
pub fn fingerprint(public_b64: &str) -> String {
    let hex = sha256_hex(public_b64.as_bytes());
    hex.as_bytes()[..16]
        .chunks(4)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join("-")
}

/// The bytes an event signature covers.
pub fn event_message(
    event_id: &str,
    node_id: &str,
    ts: i64,
    actor: &str,
    kind: &str,
    payload: &str,
) -> Vec<u8> {
    format!(
        "another-event-v1\n{event_id}\n{node_id}\n{ts}\n{actor}\n{kind}\n{}",
        sha256_hex(payload.as_bytes())
    )
    .into_bytes()
}

/// The bytes a node-to-node HTTP request signature covers.
pub fn request_message(method: &str, path_and_query: &str, time_ms: i64, body: &[u8]) -> Vec<u8> {
    format!(
        "another-request-v1\n{method}\n{path_and_query}\n{time_ms}\n{}",
        sha256_hex(body)
    )
    .into_bytes()
}

/// The bytes a pairing request signature covers, signed by the joining node's key. The hub
/// builds them from its own id and key and the secret hash it stored for the invite, so one
/// valid signature shows that the signer holds the node key, knows the hash of the invite's
/// secret, and meant this hub and this invite. Fields are joined by one newline; none may
/// contain one.
#[allow(clippy::too_many_arguments)]
pub fn pair_message(
    hub_node_id: &str,
    hub_public_key: &str,
    invite_id: &str,
    secret_hash: &str,
    node_id: &str,
    public_key: &str,
    display_name: &str,
    time_ms: i64,
) -> Vec<u8> {
    format!(
        "another-pair-v1\n{hub_node_id}\n{hub_public_key}\n{invite_id}\n{secret_hash}\n\
         {node_id}\n{public_key}\n{display_name}\n{time_ms}"
    )
    .into_bytes()
}

/// The bytes a pairing answer signature covers, signed by the hub's key. `perms` is each
/// permission's name, joined by `,` in the order the answer lists them.
pub fn pair_accept_message(
    invite_id: &str,
    secret_hash: &str,
    node_id: &str,
    public_key: &str,
    hub_display_name: &str,
    perms: &str,
) -> Vec<u8> {
    format!(
        "another-pair-accept-v1\n{invite_id}\n{secret_hash}\n{node_id}\n{public_key}\n\
         {hub_display_name}\n{perms}"
    )
    .into_bytes()
}

pub fn sign(key: &SigningKey, message: &[u8]) -> String {
    b64(&key.sign(message).to_bytes())
}

pub fn verify(public_b64: &str, message: &[u8], signature_b64: &str) -> bool {
    let Ok(key) = decode_public(public_b64) else {
        return false;
    };
    let Ok(sig_bytes) = unb64(signature_b64) else {
        return false;
    };
    let Ok(sig) = Signature::from_slice(&sig_bytes) else {
        return false;
    };
    key.verify(message, &sig).is_ok()
}

/// [`verify`] with Ed25519's strict rules: a small-order (weak) key or a non-canonical or
/// small-order signature component is refused. Signed requests between nodes and both
/// pairing signatures use it, because accepting a weak key would let a signature verify for
/// every message.
pub fn verify_strict(public_b64: &str, message: &[u8], signature_b64: &str) -> bool {
    let Ok(key) = decode_public(public_b64) else {
        return false;
    };
    if key.is_weak() {
        return false;
    }
    let Ok(sig_bytes) = unb64(signature_b64) else {
        return false;
    };
    let Ok(sig) = Signature::from_slice(&sig_bytes) else {
        return false;
    };
    key.verify_strict(message, &sig).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify() {
        let key = generate_key().unwrap();
        let public = public_of(&key);
        let msg = event_message("E", "N", 1, "a", "k", "{}");
        let sig = sign(&key, &msg);
        assert!(verify(&public, &msg, &sig));
        let tampered = event_message("E", "N", 1, "a", "k", "{\"x\":1}");
        assert!(!verify(&public, &tampered, &sig));
        let other = public_of(&generate_key().unwrap());
        assert!(!verify(&other, &msg, &sig));
        assert!(!verify("garbage", &msg, &sig));
        let round = decode_secret(&b64(key.as_bytes())).unwrap();
        assert_eq!(public_of(&round), public);
        assert_eq!(fingerprint(&public).len(), 19);
    }

    #[test]
    fn pairing_messages_are_the_documented_bytes() {
        assert_eq!(
            pair_message("H", "HK", "I", "S", "N", "NK", "name", -5),
            b"another-pair-v1\nH\nHK\nI\nS\nN\nNK\nname\n-5"
        );
        assert_eq!(
            pair_accept_message("I", "S", "N", "NK", "hub", "read_feed,send_mail"),
            b"another-pair-accept-v1\nI\nS\nN\nNK\nhub\nread_feed,send_mail"
        );
    }
}
