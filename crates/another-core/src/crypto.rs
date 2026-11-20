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

/// A nonce for one push or pull request: 16 bytes from the system's randomness, written as
/// 32 lowercase hex characters. Every request gets a new one, a retry included.
pub fn request_nonce() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|e| Error::Invalid(format!("no system randomness available: {e}")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
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

/// The bytes a hub's signature on a `200` answer to a push or a pull covers, signed by the
/// hub's key. `nonce` is the request's `x-another-nonce` header and `request_signature` the
/// text of its `x-another-signature` header, which commits to the method, the path with its
/// query, the request time and the request body; so one answer verifies for one request
/// only. `body` is the answer body as the hub serialized it, before any content encoding.
/// Fields are joined by one newline; none may contain one.
pub fn answer_message(
    hub_node_id: &str,
    spoke_node_id: &str,
    nonce: &str,
    request_signature: &str,
    body: &[u8],
) -> Vec<u8> {
    format!(
        "another-answer-v1\n{hub_node_id}\n{spoke_node_id}\n{nonce}\n{request_signature}\n{}",
        sha256_hex(body)
    )
    .into_bytes()
}

/// The bytes a hub's signature on an invite covers, signed by the hub's key: every field of
/// the invite, the format number and the address included. `secret_hash` is the SHA-256 of
/// the invite's secret, and `perms` each permission's name joined by `,`. Fields are joined
/// by one newline; none may contain one.
#[allow(clippy::too_many_arguments)]
pub fn invite_message(
    v: u8,
    hub_node_id: &str,
    hub_public_key: &str,
    hub_display_name: &str,
    hub_url: &str,
    invite_id: &str,
    secret_hash: &str,
    perms: &str,
    expires_at: i64,
) -> Vec<u8> {
    format!(
        "another-invite-v1\n{v}\n{hub_node_id}\n{hub_public_key}\n{hub_display_name}\n\
         {hub_url}\n{invite_id}\n{secret_hash}\n{perms}\n{expires_at}"
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

    #[test]
    fn answer_message_is_the_documented_bytes() {
        assert_eq!(
            answer_message("H", "S", "0a", "SIG", b"{}"),
            b"another-answer-v1\nH\nS\n0a\nSIG\n\
              44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
        );
    }

    #[test]
    fn invite_message_is_the_documented_bytes() {
        assert_eq!(
            invite_message(
                2,
                "H",
                "HK",
                "hub",
                "https://hub.example",
                "I",
                "S",
                "read_feed,send_mail",
                -5
            ),
            b"another-invite-v1\n2\nH\nHK\nhub\nhttps://hub.example\nI\nS\nread_feed,send_mail\n-5"
        );
    }
}
