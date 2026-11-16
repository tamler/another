//! Controlled-tool operation binding and durable outcome accounting.
//! Client call IDs correlate cooperating sessions; they are not authentication.

use super::*;

pub const CONTROLLED_EDIT_TOOL: &str = "controlled_edit";
pub const MAX_CONTROLLED_BYTES: usize = 1024 * 1024;

/// One existing literal file, replaced only when its complete preimage matches.
#[derive(Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ControlledEdit {
    pub project: String,
    pub path: String,
    pub expected_sha256: String,
    pub replacement: String,
}

impl ControlledEdit {
    pub fn validate(&self) -> Result<()> {
        if !std::path::Path::new(&self.project).is_absolute()
            || self.project.len() > 4096
            || self.project.chars().any(char::is_control)
            || self.path.len() > 4096
            || self.path.split('/').count() > 128
            || self
                .path
                .split('/')
                .any(|c| c.is_empty() || c == "." || c == ".." || c.trim() != c || !c.is_ascii())
            || self
                .path
                .chars()
                .any(|c| c.is_control() || "\\:*?[]{}".contains(c))
            || self.replacement.len() > MAX_CONTROLLED_BYTES
            || self.expected_sha256.len() != 64
            || !self
                .expected_sha256
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err(Error::Invalid("invalid controlled edit arguments".into()));
        }
        Ok(())
    }

    fn digest(&self, project_id: &str) -> Result<[u8; 32]> {
        self.validate()?;
        let replacement = controlled_sha256(self.replacement.as_bytes());
        let mut hash = Sha256::new();
        for field in [
            "another-controlled-operation-v1",
            CONTROLLED_EDIT_TOOL,
            project_id,
            self.path.as_str(),
            self.expected_sha256.as_str(),
            replacement.as_str(),
        ] {
            hash.update((field.len() as u64).to_be_bytes());
            hash.update(field.as_bytes());
        }
        Ok(hash.finalize().into())
    }
}

pub fn controlled_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Trusted server state returned by an atomic claim; never accepted from tool arguments.
pub struct ControlledClaim {
    id: String,
    agent: String,
    operation: [u8; 32],
    object: String,
    client: String,
    session_digest: String,
}

impl ControlledClaim {
    pub fn admission_id(&self) -> &str {
        &self.id
    }
}

fn call_key(tx: &Tx<'_>, id: &str) -> Result<[u8; 32]> {
    use ring::hmac;
    validate_tool_use_id(id)?;
    let key = hmac::Key::new(hmac::HMAC_SHA256, &admission_key(tx)?);
    let mut hash = hmac::Context::with_key(&key);
    for field in ["another-admission-call-v1", tx.node_id, id] {
        hash.update(&(field.len() as u64).to_be_bytes());
        hash.update(field.as_bytes());
    }
    let mut out = [0; 32];
    out.copy_from_slice(hash.sign().as_ref());
    Ok(out)
}

fn bound(
    tx: &Tx<'_>,
    deadline: std::time::Instant,
    agent: &str,
    client: &str,
    digest: &str,
) -> Result<bool> {
    Ok(binding_scan(tx, deadline)?
        .iter()
        .any(|b| b.agent_id == agent && b.client == client && b.digest == digest))
}

fn target_stays_in_project(tx: &Tx<'_>, project: &str, path: &str) -> Result<bool> {
    let root: String = tx.query_row(
        "SELECT abs_path FROM projects WHERE node_id=?1 AND project_id=?2",
        params![tx.node_id, project],
        |r| r.get(0),
    )?;
    let root = std::fs::canonicalize(root)?;
    let target = root.join(path);
    let target = target
        .to_str()
        .ok_or_else(|| Error::Invalid("invalid controlled root".into()))?
        .replace('\\', "/")
        .to_lowercase();
    let mut stmt =
        tx.prepare("SELECT project_id,abs_path FROM projects WHERE node_id=?1 LIMIT 1001")?;
    let rows = stmt.query_map([tx.node_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    for (count, row) in rows.enumerate() {
        if count >= 1000 {
            return Err(Error::IdentityScanBudgetExceeded(
                "project scope row limit".into(),
            ));
        }
        let (id, path) = row?;
        if id == project {
            continue;
        }
        let resolved = match std::fs::canonicalize(&path) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let Some(path) = resolved.to_str() else {
            return Ok(false);
        };
        let other = path.replace('\\', "/").to_lowercase();
        if target == other || target.starts_with(&format!("{}/", other.trim_end_matches('/'))) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn live_holds(tx: &Tx<'_>, id: &str, agent: &str, target_hash: &[u8]) -> Result<bool> {
    let rows = {
        let mut stmt = tx.prepare("SELECT h.hold_id,h.project_id,h.pattern,h.node_id,h.agent_id,h.expires_at,h.released_at,h.exclusive,h.created_at,h.ttl_seconds
            FROM admission_holds ah JOIN holds h ON h.hold_id=ah.hold_id WHERE ah.admission_id=?1 LIMIT 2")?;
        let rows = stmt.query_map([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, Option<i64>>(6)?,
                r.get::<_, bool>(7)?,
                r.get::<_, i64>(8)?,
                r.get::<_, i64>(9)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mapped: i64 = tx.query_row(
        "SELECT count(*) FROM (SELECT hold_id FROM admission_holds WHERE admission_id=?1 LIMIT 2)",
        [id],
        |r| r.get(0),
    )?;
    if mapped != 1 || rows.len() != 1 {
        return Ok(false);
    }
    let (hold, project, path, node, owner, expires, released, exclusive, created, ttl) = &rows[0];
    if node != tx.node_id
        || owner != agent
        || *expires <= tx.now
        || released.is_some()
        || !exclusive
        || *ttl != crate::holds::DEFAULT_HOLD_TTL_SECONDS
        || created.checked_add(ttl.saturating_mul(1000)) != Some(*expires)
    {
        return Ok(false);
    }
    if !target_stays_in_project(tx, project, path)? {
        return Ok(false);
    }
    Ok(
        admission_target_hash(&admission_key(tx)?, id, vec![(hold, project, path)])?.as_slice()
            == target_hash,
    )
}

impl Store {
    /// Pre-tool admission for cooperating sessions. Does not authorize a native editor.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_controlled_edit(
        &self,
        client: &str,
        program: Program,
        root: &str,
        requested_actor: Option<(&str, bool)>,
        session_id: &str,
        tool_id: &str,
        edit: &ControlledEdit,
    ) -> Result<String> {
        validate_session_id(session_id)?;
        validate_tool_use_id(tool_id)?;
        edit.validate()?;
        if !matches!(client, "claude" | "codex") || root != edit.project {
            return Err(Error::Invalid(
                "unsupported controlled edit client or project".into(),
            ));
        }
        self.write_identity(|tx, deadline| {
            let (address, digest) = resolve_session_actor_tx(tx, deadline, client, program, root, requested_actor, session_id).map_err(|error|match error {
                Error::Invalid(_) | Error::Forbidden(_) | Error::NotFound(_) | Error::Ambiguous(_)=>Error::IdentityUnavailable("controlled edit session binding unavailable".into()),
                other=>other,
            })?;
            let me = resolve_agent(tx, tx.node_id, &address)?;
            let project = find_project(tx, tx.node_id, root)?;
            if !target_stays_in_project(tx,&project.project_id,&edit.path)? {
                return Err(Error::Invalid("controlled edit reaches another registered project".into()));
            }
            let operation = edit.digest(&project.project_id)?;
            let key = call_key(tx, tool_id)?;
            let seen: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM admissions WHERE node_id=?1 AND call_key=?2)", params![tx.node_id,&key[..]], |r| r.get(0))?;
            if seen { return Err(Error::Invalid("controlled edit call was already seen".into())); }
            let plan = crate::holds::guard_plan_controlled_tx(tx, &me, project, std::slice::from_ref(&edit.path))?;
            if !plan.conflicts.is_empty() {
                let blocked=crate::holds::guard_blocked_tx(tx,&me,plan)?;
                let held=&blocked.conflicts[0].held;
                return Ok(Err(Error::Conflict(format!("controlled edit is held by {} on {} until {}; coordinate before retrying",held.agent,held.pattern,held.expires_at))));
            }
            let id = new_id();
            let correlation = admission_correlation(tx, client, &me.id, &digest, tool_id)?;
            tx.emit(&address, "admission.created", json!({"admission_id":id}))?;
            tx.execute("INSERT INTO admissions(admission_id,node_id,agent_id,correlation,targets,state,created_at,call_key,operation,client,session_digest)
                VALUES(?1,?2,?3,?4,?5,'pending',?6,?7,?8,?9,?10)",
                params![id,tx.node_id,me.id,&correlation[..],&[0u8;32][..],tx.now,&key[..],&operation[..],client,digest])?;
            let holds = crate::holds::guard_take_pending_tx(tx, &me, plan, &id, "controlled edit")?;
            let targets = admission_target_hash(&admission_key(tx)?, &id, holds.iter().map(|(h,p,f)| (h.as_str(),p.as_str(),f.as_str())).collect())?;
            tx.execute("UPDATE admissions SET targets=?2 WHERE admission_id=?1",params![id,&targets[..]])?;
            Ok(Ok(id))
        })?
    }

    /// Atomically consume one pending call and persist its exact object intent before I/O.
    pub fn claim_controlled_edit(
        &self,
        client: &str,
        tool_id: &str,
        edit: &ControlledEdit,
        object: &str,
    ) -> Result<ControlledClaim> {
        edit.validate()?;
        if object.is_empty()
            || object.len() > 128
            || !object.bytes().all(|b| b.is_ascii_digit() || b == b':')
        {
            return Err(Error::Invalid("invalid controlled object identity".into()));
        }
        self.write_identity(|tx, deadline| {
            let key = call_key(tx, tool_id)?;
            let project = find_project(tx, tx.node_id, &edit.project)?;
            let operation = edit.digest(&project.project_id)?;
            let row = tx.query_row("SELECT admission_id,agent_id,operation,targets,client,session_digest,created_at FROM admissions
                WHERE node_id=?1 AND call_key=?2 AND state='pending'",params![tx.node_id,&key[..]], |r| Ok((
                    r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Vec<u8>>(2)?,r.get::<_,Vec<u8>>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,i64>(6)?))).optional()?;
            let Some((id,agent,stored,targets,stored_client,digest,created)) = row else { return Err(Error::Invalid("no unclaimed controlled edit admission".into())); };
            if stored != operation || stored_client != client || created > tx.now || tx.now-created>120_000
                || !bound(tx,deadline,&agent,client,&digest)? || !live_holds(tx,&id,&agent,&targets)? {
                return Err(Error::Invalid("controlled edit admission does not match".into()));
            }
            tx.emit("system", "admission.claimed", json!({"admission_id":id}))?;
            let changed = tx.execute("UPDATE admissions SET state='claimed',claimed_at=?2,object_identity=?3,preimage=?4,replacement=?5
                WHERE admission_id=?1 AND state='pending'", params![id,tx.now,object,edit.expected_sha256,controlled_sha256(edit.replacement.as_bytes())])?;
            if changed != 1 { return Err(Error::Invalid("controlled edit claim was consumed".into())); }
            Ok(ControlledClaim {id,agent,operation,object:object.into(),client:client.into(),session_digest:digest})
        })
    }

    /// Only the controlled server calls this after its write, sync and checked resolution.
    /// A false or unverifiable result becomes permanent unknown evidence, never success.
    pub fn finish_controlled_edit(&self, claim: &ControlledClaim, verified: bool) -> Result<bool> {
        self.write_identity(|tx,deadline| {
            let row = tx.query_row("SELECT state,agent_id,operation,object_identity,targets,client,session_digest FROM admissions WHERE node_id=?1 AND admission_id=?2",
                params![tx.node_id,claim.id], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Vec<u8>>(2)?,r.get::<_,String>(3)?,r.get::<_,Vec<u8>>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?)))?;
            let (state,agent,operation,object,targets,client,digest) = row;
            if state != "claimed" { return Ok(false); }
            let success = verified && agent==claim.agent && operation==claim.operation && object==claim.object
                && client==claim.client && digest==claim.session_digest && bound(tx,deadline,&agent,&client,&digest)?
                && live_holds(tx,&claim.id,&agent,&targets)?;
            let next = if success { "confirmed" } else { "unknown" };
            tx.emit("system", &format!("admission.{next}"), json!({"admission_id":claim.id}))?;
            tx.execute("UPDATE admissions SET state=?2,settled_at=?3 WHERE admission_id=?1 AND state='claimed'", params![claim.id,next,tx.now])?;
            if success {
                let hold:String=tx.query_row("SELECT hold_id FROM admission_holds WHERE admission_id=?1",[&claim.id],|r|r.get(0))?;
                tx.emit("system", "hold.renewed", json!({"hold_ids":[hold]}))?;
                tx.execute("UPDATE holds SET renewed_at=?2,expires_at=?2+ttl_seconds*1000 WHERE hold_id IN (SELECT hold_id FROM admission_holds WHERE admission_id=?1)",params![claim.id,tx.now])?;
            }
            Ok(success)
        })
    }

    /// Durable unknown outcomes remain visible even after their temporary holds lapse.
    pub fn unknown_controlled_edits(&self) -> Result<Vec<String>> {
        self.read(|conn| {
            let mut stmt = conn.prepare("SELECT admission_id FROM admissions WHERE node_id=?1 AND state='unknown' ORDER BY created_at LIMIT 1000")?;
            let rows = stmt.query_map([self.node_id()], |r| r.get(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn unknown_controlled_edit_count(&self) -> Result<u64> {
        self.read(|conn| {
            let count: i64 = conn.query_row(
                "SELECT count(*) FROM admissions WHERE node_id=?1 AND state='unknown'",
                [self.node_id()],
                |r| r.get(0),
            )?;
            u64::try_from(count)
                .map_err(|_| Error::Invalid("invalid unknown admission count".into()))
        })
    }
}
