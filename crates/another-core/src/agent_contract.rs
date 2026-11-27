//! Versioned private-channel facade over the actor/lease core. A name, native id,
//! process id or environment selector is never accepted in place of a capability.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    ActorProof, CheckoutRight, EditOutcome, Error, LeaseRef, ParticipantProof, Result, Store,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentEnvelope {
    pub major: u32,
    pub minor: u32,
    pub request_id: String,
    #[serde(default)]
    pub required_features: Vec<String>,
    pub operation: AgentOperation,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentOperation {
    Check {
        actor: ActorProof,
        lease: LeaseRef,
    },
    Resolve {
        participant: ParticipantProof,
        native_key: String,
    },
    CheckEdit {
        actor: ActorProof,
        call_id: String,
    },
    Compatibility {
        actor: ActorProof,
        action: CompatibilityAction,
    },
    Register {
        participant: ParticipantProof,
        native_key: String,
        label: String,
        parent: Option<ActorProof>,
    },
    Resume {
        actor: ActorProof,
        parent: Option<ActorProof>,
    },
    Checkout {
        actor: ActorProof,
        checkout: CheckoutRight,
    },
    Renew {
        actor: ActorProof,
        lease: LeaseRef,
    },
    Release {
        actor: ActorProof,
        lease: LeaseRef,
    },
    EditBegin {
        actor: ActorProof,
        targets: Vec<String>,
        ttl_seconds: Option<i64>,
    },
    EditComplete {
        actor: ActorProof,
        call_id: String,
        outcome: EditOutcome,
    },
    Activity {
        actor: ActorProof,
    },
    SessionEnd {
        actor: ActorProof,
    },
    Status,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum CompatibilityAction {
    Hold {
        project: String,
        paths: Vec<String>,
        ttl_seconds: Option<i64>,
    },
    RenewHolds {
        hold_ids: Vec<String>,
    },
    ReleaseHolds {
        hold_ids: Vec<String>,
    },
    RegisterResource {
        name: String,
        kind: crate::ResourceKind,
        endpoint: Option<String>,
        ttl_seconds: Option<i64>,
    },
    Borrow {
        resource_id: String,
        ttl_seconds: Option<i64>,
    },
    ReleaseResource {
        resource_id: String,
        lease: LeaseRef,
    },
    Next {
        ttl_seconds: Option<i64>,
    },
    Done {
        item: String,
        summary: String,
        commit: String,
        lease: LeaseRef,
    },
    Send {
        to: Vec<String>,
        subject: String,
        body: String,
        #[serde(default)]
        create_request: bool,
        due_in_seconds: Option<i64>,
    },
    Request {
        request_id: String,
        request_action: crate::RequestAction,
        ttl_seconds: Option<i64>,
        lease: Option<LeaseRef>,
        note: Option<String>,
    },
    ClaimRequest {
        request_id: String,
    },
}

impl Store {
    /// The caller transports this envelope over a private integration channel. Do
    /// not log it: registration and actor requests contain private capabilities.
    pub fn agent_contract(&self, request: AgentEnvelope) -> Result<Value> {
        if request.major != 1
            || request
                .required_features
                .iter()
                .any(|f| !matches!(f.as_str(), "leases" | "session_end" | "exact_targets"))
        {
            return Err(Error::Invalid(
                "INCOMPATIBLE_VERSION: expected another.agent/1 and supported required features"
                    .into(),
            ));
        }
        if request.request_id.is_empty()
            || request.request_id.len() > 4096
            || request.request_id.contains('\0')
        {
            return Err(Error::Invalid(
                "request_id must be 1-4096 bytes without NUL".into(),
            ));
        }
        let id = &request.request_id;
        let reminder = match &request.operation {
            AgentOperation::Checkout { actor, .. }
            | AgentOperation::Renew { actor, .. }
            | AgentOperation::Release { actor, .. }
            | AgentOperation::EditBegin { actor, .. } => {
                self.contract_action_activity(actor, id, false)?
            }
            AgentOperation::EditComplete { actor, call_id, .. } => {
                self.contract_action_activity(actor, call_id, true)?
            }
            _ => None,
        };
        let result = match request.operation {
            AgentOperation::Check { actor, lease } => {
                serde_json::to_value(self.check_right(&actor, &lease)?)?
            }
            AgentOperation::Resolve {
                participant,
                native_key,
            } => serde_json::to_value(self.resolve_registered_actor(&participant, &native_key)?)?,
            AgentOperation::CheckEdit { actor, call_id } => {
                serde_json::to_value(self.check_actor_edit(&actor, &call_id)?)?
            }
            AgentOperation::Compatibility { actor, action } => {
                let fingerprint = serde_json::to_value(&action)?;
                crate::actor_leases::compatibility_call(self, &actor, id, &fingerprint, |scoped, key| {
                    // `scoped` carries the typed proof; `key` is the public actor id
                    // selecting it. Every called mutation revalidates proof and epoch
                    // inside its own transaction.
                    let key = key.to_owned();
                    let projection = match action {
                        CompatibilityAction::Hold {
                            project,
                            paths,
                            ttl_seconds,
                        } => serde_json::to_value(scoped.acquire_holds(crate::AcquireHolds {
                            agent: key,
                            project,
                            paths,
                            ttl_seconds,
                            exclusive: true,
                            reason: None,
                            repo_wide: false,
                        })?)?,
                        CompatibilityAction::RenewHolds { hold_ids } => {
                            serde_json::to_value(scoped.renew_holds(&key, &hold_ids)?)?
                        }
                        CompatibilityAction::ReleaseHolds { hold_ids } => {
                            serde_json::to_value(scoped.release_holds(&key, &hold_ids)?)?
                        }
                        CompatibilityAction::RegisterResource {
                            name,
                            kind,
                            endpoint,
                            ttl_seconds,
                        } => serde_json::to_value(scoped.register_resource(
                            crate::RegisterResource {
                                agent: key,
                                name,
                                kind,
                                endpoint,
                                ttl_seconds,
                                project: None,
                                port: None,
                                pid: None,
                                purpose: None,
                                mode: crate::LeaseMode::Shared,
                                metadata: None,
                            },
                        )?)?,
                        CompatibilityAction::Borrow {
                            resource_id,
                            ttl_seconds,
                        } => serde_json::to_value(scoped.borrow_resource(
                            &key,
                            &crate::ResourceRef::Id(resource_id),
                            ttl_seconds,
                        )?)?,
                        CompatibilityAction::ReleaseResource { resource_id, lease } => {
                            serde_json::to_value(scoped.release_resource_with_lease(
                                &key,
                                &crate::ResourceRef::Id(resource_id),
                                false,
                                Some(&lease),
                            )?)?
                        }
                        CompatibilityAction::Next { ttl_seconds } => {
                            serde_json::to_value(scoped.queue_next_with_ttl(&key, ttl_seconds)?)?
                        }
                        CompatibilityAction::Done {
                            item,
                            summary,
                            commit,
                            lease,
                        } => serde_json::to_value(scoped.queue_done_with_lease(
                            &key,
                            &item,
                            crate::DoneItem {
                                summary,
                                pointer_kind: crate::PointerKind::Commit,
                                pointer: commit,
                            },
                            Some(&lease),
                        )?)?,
                        CompatibilityAction::Send {
                            to,
                            subject,
                            body,
                            create_request,
                            due_in_seconds,
                        } => serde_json::to_value(scoped.send_message(crate::SendMessage {
                            from: key,
                            to,
                            subject,
                            body,
                            request: create_request.then_some(crate::NewRequest { due_in_seconds }),
                            ..Default::default()
                        })?)?,
                        CompatibilityAction::ClaimRequest { request_id } => {
                            serde_json::to_value(scoped.update_request(crate::RequestUpdate {
                                agent: key,
                                request_id,
                                action: crate::RequestAction::Claim,
                                due_in_seconds: None,
                                note: None,
                            })?)?
                        }
                        CompatibilityAction::Request {
                            request_id,
                            request_action,
                            ttl_seconds,
                            lease,
                            note,
                        } => serde_json::to_value(scoped.update_request_with_lease(
                            crate::RequestUpdate {
                                agent: key,
                                request_id,
                                action: request_action,
                                note,
                                due_in_seconds: None,
                            },
                            None,
                            ttl_seconds,
                            lease.as_ref(),
                        )?)?,
                    };
                    let leases: Vec<_> = self
                        .right_leases()?
                        .into_iter()
                        .filter(|l| l.owner_actor_id == actor.actor_id)
                        .collect();
                    Ok(json!({"projection":projection, "leases":leases}))
                })?
            }
            AgentOperation::Register {
                participant,
                native_key,
                label,
                parent,
            } => serde_json::to_value(self.register_actor(
                &participant,
                &native_key,
                &label,
                parent.as_ref(),
            )?)?,
            AgentOperation::Resume { actor, parent } => {
                serde_json::to_value(self.resume_actor(&actor, parent.as_ref())?)?
            }
            AgentOperation::Checkout { actor, checkout } => {
                if !matches!(
                    checkout.subject_kind,
                    crate::SubjectKind::File | crate::SubjectKind::Presence
                ) {
                    return Err(Error::Forbidden("NO_CALL_CONTEXT: business subjects require their authenticated admission/projection operation; unconverted reservation routes refuse".into()));
                }
                serde_json::to_value(self.checkout_right(&actor, id, checkout)?)?
            }
            AgentOperation::Renew { actor, lease } => {
                serde_json::to_value(self.renew_right(&actor, id, lease)?)?
            }
            AgentOperation::Release { actor, lease } => {
                serde_json::to_value(self.release_right(&actor, id, lease)?)?
            }
            AgentOperation::EditBegin {
                actor,
                targets,
                ttl_seconds,
            } => serde_json::to_value(self.begin_actor_edit(&actor, id, &targets, ttl_seconds)?)?,
            AgentOperation::EditComplete {
                actor,
                call_id,
                outcome,
            } => serde_json::to_value(self.complete_actor_edit(&actor, &call_id, outcome)?)?,
            AgentOperation::Activity { actor } => json!({"reminder": self.actor_activity(&actor)?}),
            AgentOperation::SessionEnd { actor } => {
                json!({"released": self.end_actor_session(&actor, id)?})
            }
            AgentOperation::Status => {
                json!({"actors": self.contract_actors()?, "leases": self.right_leases()?, "operations": self.actor_operations()?, "cursor": crate::CURSOR_COVERAGE})
            }
        };
        Ok(
            json!({"contract": "another.agent/1", "major": 1, "minor": 0, "request_id": request.request_id, "authority_now": self.now(), "reminder": reminder, "result": result}),
        )
    }
}
