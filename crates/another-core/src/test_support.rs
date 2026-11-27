//! For this workspace's tests only: build wire events as a node really signs them, so a
//! test can forge, re-sign or replay one. Signing goes through [`crate::crypto`]; nothing
//! here is a second implementation of the signed bytes.

use rusqlite::Connection;

use crate::federation::wire_from_row;
use crate::{crypto, WireEvent};

/// `ev`, signed with the secret key of the local node of the home `conn` is open on. The
/// incoming `sig` is ignored. `ev.node_id` should be that local node's id for the event to
/// verify on a peer; a test that wants a mismatch can set another one.
pub fn forge_event(conn: &Connection, mut ev: WireEvent) -> WireEvent {
    let secret: String = conn
        .query_row("SELECT secret_key FROM nodes WHERE is_local = 1", [], |r| {
            r.get(0)
        })
        .expect("the home has a local node with a secret key");
    let key = crypto::decode_secret(&secret).expect("the stored secret key decodes");
    ev.sig = crypto::sign(
        &key,
        &crypto::event_message(
            &ev.event_id,
            &ev.node_id,
            ev.ts,
            &ev.actor,
            &ev.kind,
            &ev.payload,
        ),
    );
    ev
}

/// Every event of origin `node_id` in the log `conn` is open on, oldest first, exactly as
/// stored and signed. An unsigned row has an empty `sig`.
pub fn wire_events_of(conn: &Connection, node_id: &str) -> Vec<WireEvent> {
    let mut stmt = conn
        .prepare(
            "SELECT seq, event_id, node_id, ts, actor, kind, payload, prev_id, sig
             FROM events WHERE node_id = ?1 ORDER BY seq",
        )
        .expect("the events table reads");
    stmt.query_map([node_id], wire_from_row)
        .expect("the events table reads")
        .collect::<rusqlite::Result<_>>()
        .expect("every event row maps to a wire event")
}
