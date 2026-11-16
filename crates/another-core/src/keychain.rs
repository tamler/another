//! Node key rotation primitives (peer-key spec, `.lanes/another-peer-keys-20261116/SPEC.md`
//! §3.2 and §3.3b). Pure functions, no database: the chain digest, the rotation proof, and
//! the checks a receiver runs on one rotation link or on a whole chain presented at pairing.
//!
//! A rotation from key `old` to key `new` is a `node.key_rotated` event signed by `old`. Its
//! payload names both keys, carries `chain` (the digest `D` of every earlier link, back to
//! the node's first key) and `proof`, a signature by `new` over the node, both keys and
//! `chain`. `proof` shows possession of `new`; `chain` binds the link to the whole history
//! before it, so nobody holding a retired key can put a fabricated prefix in front of a
//! genuine link.

use serde::{Deserialize, Serialize};

use crate::crypto;
use crate::error::{Error, Result};
use crate::federation::WireEvent;

/// The event kind of a key rotation.
pub const ROTATION_KIND: &str = "node.key_rotated";

/// The payload of a `node.key_rotated` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RotationPayload {
    pub node_id: String,
    pub old_public_key: String,
    pub new_public_key: String,
    /// `D` before this link (hex).
    pub chain: String,
    /// Signature by `new_public_key` over [`proof_message`].
    pub proof: String,
}

/// `D_0`: the digest of a node's history before any rotation, from its first key.
pub fn chain_genesis(node_id: &str, first_public_key: &str) -> String {
    crypto::sha256_hex(format!("another-key-chain-v1\n{node_id}\n{first_public_key}").as_bytes())
}

/// `D` after one more link: the link's event id and the key it introduced.
pub fn chain_next(chain: &str, event_id: &str, new_public_key: &str) -> String {
    crypto::sha256_hex(format!("{chain}\n{event_id}\n{new_public_key}").as_bytes())
}

/// The bytes the new key signs as `proof`. They include the rotation event's own id, so the
/// new key fixes the id of the link that introduces it; earlier links' ids are fixed
/// through `chain`.
pub fn proof_message(node_id: &str, event_id: &str, old: &str, new: &str, chain: &str) -> Vec<u8> {
    format!("another-key-rotation-v2\n{node_id}\n{event_id}\n{old}\n{new}\n{chain}").into_bytes()
}

/// The payload of rotation event `event_id`, moving `node_id` from `old` to the key `new`
/// at history digest `chain`. The caller signs the event itself with `old`, like any event
/// it authors.
pub fn rotation_payload(
    node_id: &str,
    event_id: &str,
    old_public_key: &str,
    new_key: &ed25519_dalek::SigningKey,
    chain: &str,
) -> RotationPayload {
    let new_public_key = crypto::public_of(new_key);
    let proof = crypto::sign(
        new_key,
        &proof_message(node_id, event_id, old_public_key, &new_public_key, chain),
    );
    RotationPayload {
        node_id: node_id.into(),
        old_public_key: old_public_key.into(),
        new_public_key,
        chain: chain.into(),
        proof,
    }
}

/// Where a receiver stands in one node's key history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPosition {
    /// The key the node's next event must be signed with (`verify_key`).
    pub key: String,
    /// `D` at this point (`key_chain`).
    pub chain: String,
}

fn invalid(why: impl Into<String>) -> Error {
    Error::Invalid(why.into())
}

/// `text` if it is a public key written in its one canonical form, the way
/// [`crypto::public_of`] writes it. Decoding is lenient (it trims whitespace), so without
/// this one key could take several spellings and slip past the no-reuse and equality
/// checks below (FINDINGS-192).
pub fn canonical_key(text: &str) -> Result<&str> {
    let key = crypto::decode_public(text)?;
    if crypto::b64(key.as_bytes()) == text {
        Ok(text)
    } else {
        Err(invalid("public key is not in canonical form"))
    }
}

/// Check one rotation event against the receiver's position for its origin node, and
/// return the position after it. Checks, in order: it is a rotation; its payload parses and
/// names the event's own origin; the event is signed by the current key; it rotates from
/// that key; `chain` matches; the new key is not one already used (`earlier`); the proof
/// verifies with the new key. Nothing is stored here.
pub fn verify_link(ev: &WireEvent, at: &KeyPosition, earlier: &[String]) -> Result<KeyPosition> {
    if ev.kind != ROTATION_KIND {
        return Err(invalid(format!("{} is not a key rotation", ev.event_id)));
    }
    let p: RotationPayload = serde_json::from_str(&ev.payload)
        .map_err(|e| invalid(format!("rotation {}: bad payload: {e}", ev.event_id)))?;
    if p.node_id != ev.node_id {
        return Err(invalid(format!(
            "rotation {} names node {} but was signed by {}",
            ev.event_id, p.node_id, ev.node_id
        )));
    }
    // Every key is compared as text below, so each must have exactly one spelling.
    for k in [&at.key, &p.old_public_key, &p.new_public_key] {
        canonical_key(k).map_err(|e| invalid(format!("rotation {}: {e}", ev.event_id)))?;
    }
    let signed = crypto::event_message(
        &ev.event_id,
        &ev.node_id,
        ev.ts,
        &ev.actor,
        &ev.kind,
        &ev.payload,
    );
    if !crypto::verify(&at.key, &signed, &ev.sig) {
        return Err(invalid(format!(
            "rotation {} is not signed by the node's current key",
            ev.event_id
        )));
    }
    if p.old_public_key != at.key {
        return Err(invalid(format!(
            "rotation {} starts from a key that is not the current one",
            ev.event_id
        )));
    }
    if p.chain != at.chain {
        return Err(invalid(format!(
            "rotation {} commits to a different key history",
            ev.event_id
        )));
    }
    if p.new_public_key == at.key || earlier.contains(&p.new_public_key) {
        return Err(invalid(format!(
            "rotation {} goes back to a key already used",
            ev.event_id
        )));
    }
    let msg = proof_message(
        &p.node_id,
        &ev.event_id,
        &p.old_public_key,
        &p.new_public_key,
        &p.chain,
    );
    if !crypto::verify(&p.new_public_key, &msg, &p.proof) {
        return Err(invalid(format!(
            "rotation {} has no valid proof from its new key",
            ev.event_id
        )));
    }
    Ok(KeyPosition {
        key: p.new_public_key.clone(),
        chain: chain_next(&p.chain, &ev.event_id, &p.new_public_key),
    })
}

/// A chain presented at pairing, verified (§3.3b).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedChain {
    /// The node's first key: where a receiver that has never seen it starts verifying.
    pub first_key: String,
    /// The position after the last link; its key is the trusted current key.
    pub end: KeyPosition,
}

/// Verify a whole rotation chain for `node_id`, oldest link first, against the key the
/// receiver trusts as current (the invite's hub key, or the joining spoke's presented key).
/// Every link must pass [`verify_link`] from the position the links before it lead to,
/// starting at the first link's old key, and the last must end at `trusted_current`. An
/// empty chain means the trusted key is the first key.
pub fn verify_chain(
    node_id: &str,
    links: &[WireEvent],
    trusted_current: &str,
) -> Result<VerifiedChain> {
    canonical_key(trusted_current)?;
    let first_key = match links.first() {
        None => trusted_current.to_string(),
        Some(ev) => {
            serde_json::from_str::<RotationPayload>(&ev.payload)
                .map_err(|e| invalid(format!("rotation {}: bad payload: {e}", ev.event_id)))?
                .old_public_key
        }
    };
    let mut at = KeyPosition {
        key: first_key.clone(),
        chain: chain_genesis(node_id, &first_key),
    };
    let mut used = vec![first_key.clone()];
    for ev in links {
        if ev.node_id != node_id {
            return Err(invalid(format!(
                "rotation {} belongs to node {}, not {node_id}",
                ev.event_id, ev.node_id
            )));
        }
        at = verify_link(ev, &at, &used)?;
        used.push(at.key.clone());
    }
    if at.key != trusted_current {
        return Err(invalid(format!(
            "the key chain for {node_id} does not end at the trusted current key"
        )));
    }
    Ok(VerifiedChain { first_key, end: at })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    const NODE: &str = "01NODE";

    fn key() -> SigningKey {
        crypto::generate_key().unwrap()
    }

    /// A rotation event from `old` to `new`, as the node would author it.
    fn rotation(id: &str, old: &SigningKey, new: &SigningKey, chain: &str) -> WireEvent {
        let payload = serde_json::to_string(&rotation_payload(
            NODE,
            id,
            &crypto::public_of(old),
            new,
            chain,
        ))
        .unwrap();
        signed(id, old, payload)
    }

    fn signed(id: &str, by: &SigningKey, payload: String) -> WireEvent {
        let ts = 1_800_000_000_000;
        let msg = crypto::event_message(id, NODE, ts, "system", ROTATION_KIND, &payload);
        WireEvent {
            seq: 0,
            event_id: id.into(),
            node_id: NODE.into(),
            ts,
            actor: "system".into(),
            kind: ROTATION_KIND.into(),
            payload,
            prev_id: None,
            sig: crypto::sign(by, &msg),
        }
    }

    /// K1 -> K2 -> K3, each link committing to the history before it.
    fn two_rotations() -> (Vec<SigningKey>, Vec<WireEvent>) {
        let k = vec![key(), key(), key()];
        let d0 = chain_genesis(NODE, &crypto::public_of(&k[0]));
        let r1 = rotation("R1", &k[0], &k[1], &d0);
        let d1 = chain_next(&d0, "R1", &crypto::public_of(&k[1]));
        let r2 = rotation("R2", &k[1], &k[2], &d1);
        (k, vec![r1, r2])
    }

    #[test]
    fn a_genuine_chain_verifies_to_its_trusted_end() {
        let (k, links) = two_rotations();
        let v = verify_chain(NODE, &links, &crypto::public_of(&k[2])).unwrap();
        assert_eq!(v.first_key, crypto::public_of(&k[0]));
        assert_eq!(v.end.key, crypto::public_of(&k[2]));
        // Link by link from the genesis gives the same end.
        let mut at = KeyPosition {
            key: crypto::public_of(&k[0]),
            chain: chain_genesis(NODE, &crypto::public_of(&k[0])),
        };
        for ev in &links {
            at = verify_link(ev, &at, &[]).unwrap();
        }
        assert_eq!(at, v.end);
        // An empty chain: the trusted key is the first key.
        let lone = verify_chain(NODE, &[], &crypto::public_of(&k[0])).unwrap();
        assert_eq!(lone.first_key, crypto::public_of(&k[0]));
        assert_eq!(
            lone.end.chain,
            chain_genesis(NODE, &crypto::public_of(&k[0]))
        );
    }

    /// REVIEW-R3 finding 1: someone holding the retired K1 builds E -> K1 (signed by E,
    /// proved by K1) in front of the genuine K1 -> K2. Every signature checks out and the
    /// chain ends at the trusted K2, but the genuine link committed to a history that
    /// started at K1, so the digest does not match.
    #[test]
    fn a_fabricated_prefix_before_a_genuine_link_is_refused() {
        let k1 = key();
        let k2 = key();
        let e = key();
        let d0_k1 = chain_genesis(NODE, &crypto::public_of(&k1));
        let genuine = rotation("R1", &k1, &k2, &d0_k1);
        let d0_e = chain_genesis(NODE, &crypto::public_of(&e));
        let forged = rotation("F0", &e, &k1, &d0_e);
        assert!(verify_chain(
            NODE,
            std::slice::from_ref(&genuine),
            &crypto::public_of(&k2)
        )
        .is_ok());
        let err = verify_chain(NODE, &[forged, genuine], &crypto::public_of(&k2)).unwrap_err();
        assert!(err.to_string().contains("different key history"), "{err}");
    }

    #[test]
    fn a_chain_ending_elsewhere_or_with_a_gap_is_refused() {
        let (k, links) = two_rotations();
        let wrong_end = verify_chain(NODE, &links, &crypto::public_of(&k[1])).unwrap_err();
        assert!(
            wrong_end.to_string().contains("does not end"),
            "{wrong_end}"
        );
        // R2 alone starts at K2, but its chain field committed to R1 before it.
        let gap = verify_chain(NODE, &links[1..], &crypto::public_of(&k[2])).unwrap_err();
        assert!(gap.to_string().contains("different key history"), "{gap}");
        // Out of order.
        let swapped = verify_chain(
            NODE,
            &[links[1].clone(), links[0].clone()],
            &crypto::public_of(&k[2]),
        );
        assert!(swapped.is_err());
    }

    #[test]
    fn a_link_must_be_signed_by_the_current_key_and_proved_by_the_new_one() {
        let k1 = key();
        let k2 = key();
        let other = key();
        let d0 = chain_genesis(NODE, &crypto::public_of(&k1));
        let at = KeyPosition {
            key: crypto::public_of(&k1),
            chain: d0.clone(),
        };
        // Signed by someone else.
        let payload = serde_json::to_string(&rotation_payload(
            NODE,
            "R1",
            &crypto::public_of(&k1),
            &k2,
            &d0,
        ))
        .unwrap();
        let wrong_signer = signed("R1", &other, payload.clone());
        assert!(verify_link(&wrong_signer, &at, &[])
            .unwrap_err()
            .to_string()
            .contains("not signed by the node's current key"));
        // A proof by a key other than the new one.
        let mut p: RotationPayload = serde_json::from_str(&payload).unwrap();
        p.proof = crypto::sign(
            &other,
            &proof_message(NODE, "R1", &p.old_public_key, &p.new_public_key, &p.chain),
        );
        let bad_proof = signed("R1", &k1, serde_json::to_string(&p).unwrap());
        assert!(verify_link(&bad_proof, &at, &[])
            .unwrap_err()
            .to_string()
            .contains("no valid proof"));
        // The genuine link passes.
        assert!(verify_link(&signed("R1", &k1, payload.clone()), &at, &[]).is_ok());
        // Post-R7 crypto check: the same genuine payload (and K2's genuine proof) under
        // another event id, re-signed by the retired K1, is refused: the proof covers the id.
        let renamed = signed("R1-other", &k1, payload);
        assert!(verify_link(&renamed, &at, &[])
            .unwrap_err()
            .to_string()
            .contains("no valid proof"));
    }

    #[test]
    fn a_link_cannot_name_another_node_or_go_back_to_an_old_key() {
        let k1 = key();
        let k2 = key();
        let d0 = chain_genesis(NODE, &crypto::public_of(&k1));
        let at = KeyPosition {
            key: crypto::public_of(&k1),
            chain: d0.clone(),
        };
        let mut p = rotation_payload(NODE, "R1", &crypto::public_of(&k1), &k2, &d0);
        p.node_id = "01OTHER".into();
        let elsewhere = signed("R1", &k1, serde_json::to_string(&p).unwrap());
        assert!(verify_link(&elsewhere, &at, &[])
            .unwrap_err()
            .to_string()
            .contains("names node"));
        let back = signed(
            "R1",
            &k1,
            serde_json::to_string(&rotation_payload(
                NODE,
                "R1",
                &crypto::public_of(&k1),
                &k2,
                &d0,
            ))
            .unwrap(),
        );
        let used = vec![crypto::public_of(&k2)];
        assert!(verify_link(&back, &at, &used)
            .unwrap_err()
            .to_string()
            .contains("already used"));
    }

    /// Adversarial (FINDINGS-192): `decode_public` trims whitespace, so " K0" is another
    /// spelling of the retired K0. A link K1 -> " K0" must be refused as a reused key.
    #[test]
    fn adv192_a_retired_key_spelled_with_a_space_is_still_reused() {
        let (k0, k1) = (key(), key());
        let d0 = chain_genesis(NODE, &crypto::public_of(&k0));
        let r1 = rotation("R1", &k0, &k1, &d0);
        let mut at = KeyPosition {
            key: crypto::public_of(&k0),
            chain: d0,
        };
        let mut used = vec![at.key.clone()];
        at = verify_link(&r1, &at, &used).unwrap();
        used.push(at.key.clone());
        // K1 rotates "back" to K0, spelled with a leading space. The proof is by K0.
        let alias = format!(" {}", crypto::public_of(&k0));
        let proof = crypto::sign(&k0, &proof_message(NODE, "R2", &at.key, &alias, &at.chain));
        let payload = serde_json::to_string(&RotationPayload {
            node_id: NODE.into(),
            old_public_key: at.key.clone(),
            new_public_key: alias,
            chain: at.chain.clone(),
            proof,
        })
        .unwrap();
        let back = signed("R2", &k1, payload);
        let got = verify_link(&back, &at, &used);
        assert!(
            got.is_err(),
            "reused K0 accepted as {:?}",
            got.map(|p| p.key)
        );
    }
}
