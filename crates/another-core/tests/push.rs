//! Push: scope-filtered event reads, webhook queueing / filters / retries / dead letters,
//! the new hold.conflict and resource.lease_expiring events, and upgrade from schema v6.

use another_core::*;

fn store_with_clock() -> (Store, std::sync::Arc<std::sync::atomic::AtomicI64>) {
    let (clock, handle) = Clock::manual(1_700_000_000_000);
    let s = Store::open_in_memory(clock).unwrap();
    s.ensure_project("t", "/work/shop").unwrap();
    s.ensure_project("t", "/work/vault").unwrap();
    for (name, project) in [
        ("manager", None),
        ("shopper", Some("/work/shop")),
        ("vaulter", Some("/work/vault")),
    ] {
        s.register_agent(RegisterAgent {
            name: name.into(),
            program: Program::Claude,
            project: project.map(Into::into),
            model: None,
            description: None,
        })
        .unwrap();
    }
    (s, handle)
}

fn advance(h: &std::sync::Arc<std::sync::atomic::AtomicI64>, ms: i64) {
    h.fetch_add(ms, std::sync::atomic::Ordering::SeqCst);
}

fn new_lane(s: &Store, name: &str, project: &str, lead: &str) {
    s.create_lane(
        NewLane {
            agent: "manager".into(),
            name: name.into(),
            project: project.into(),
            lead: lead.into(),
            charter: String::new(),
            status: None,
        },
        None,
    )
    .unwrap();
}

fn set_status(s: &Store, lane: &str, status: LaneStatus) {
    s.update_lane(
        LaneUpdate {
            agent: "manager".into(),
            lane: lane.into(),
            status: Some(status),
            ..Default::default()
        },
        None,
    )
    .unwrap();
}

fn hold(s: &Store, agent: &str, project: &str, path: &str) -> HoldReport {
    s.acquire_holds(AcquireHolds {
        agent: agent.into(),
        project: project.into(),
        paths: vec![path.into()],
        exclusive: true,
        reason: None,
        ttl_seconds: None,
    })
    .unwrap()
}

fn dev_server(agent: &str, ttl: Option<i64>) -> RegisterResource {
    RegisterResource {
        agent: agent.into(),
        kind: ResourceKind::DevServer,
        name: "vite@5173".into(),
        endpoint: None,
        port: None,
        pid: None,
        project: Some("shop".into()),
        purpose: None,
        mode: LeaseMode::default(),
        ttl_seconds: ttl,
        metadata: None,
    }
}

fn all_items(s: &Store, scope: Option<&TokenScope>) -> Vec<ActivityItem> {
    s.activity_scoped(
        ActivityQuery {
            since: Some(Since::Time(0)),
            limit: 500,
            kinds: vec![],
        },
        scope,
    )
    .unwrap()
    .items
}

#[test]
fn scoped_feed_hides_other_projects() {
    let (s, _) = store_with_clock();
    new_lane(&s, "shop-lane", "shop", "shopper");
    new_lane(&s, "vault-lane", "vault", "vaulter");
    hold(&s, "vaulter@vault", "vault", "src/secret.rs");
    hold(&s, "shopper@shop", "shop", "src/cart.rs");
    s.send_message(SendMessage {
        from: "vaulter@vault".into(),
        to: vec!["vaulter@vault".into()],
        subject: "vault-only note".into(),
        body: "x".into(),
        ..Default::default()
    })
    .unwrap();
    s.create_token("manager", &["/work/vault".into()], None, None)
        .unwrap();
    let t = s
        .create_token("shopper@shop", &["/work/shop".into()], None, None)
        .unwrap();
    let scope = s.verify_token(&t.secret).unwrap();

    let seen = all_items(&s, Some(&scope));
    let text: Vec<&str> = seen.iter().map(|i| i.summary.as_str()).collect();
    assert!(text.iter().any(|t| t.contains("shop-lane")), "{text:?}");
    assert!(text.iter().any(|t| t.contains("cart.rs")), "{text:?}");
    for leak in ["vault-lane", "secret.rs", "vault-only note", "/work/vault"] {
        assert!(
            !text.iter().any(|t| t.contains(leak)),
            "{leak} leaked: {text:?}"
        );
    }
    assert!(
        !seen
            .iter()
            .any(|i| i.kind.starts_with("token.") || i.kind.starts_with("node.")),
        "token and node events are never visible to scoped callers"
    );
    // Unscoped (local) callers still see everything.
    let everything = all_items(&s, None);
    assert!(everything.iter().any(|i| i.summary.contains("vault-lane")));
    assert!(everything.len() > seen.len());

    // The cursor moves past hidden events, so a scoped reader never re-scans them.
    let feed = s
        .activity_scoped(
            ActivityQuery {
                since: Some(Since::Time(0)),
                limit: 500,
                kinds: vec![],
            },
            Some(&scope),
        )
        .unwrap();
    assert_eq!(
        feed.next_since.as_deref(),
        s.latest_event_id().unwrap().as_deref()
    );

    // The summary only counts what the scope may see.
    let sum = s
        .activity_summary_scoped(Some(Since::Time(0)), Some(&scope))
        .unwrap();
    assert_eq!(sum.live_holds, 1);
    assert!(sum.agents.iter().all(|a| a.agent != "vaulter@vault"));
    assert!(!sum.text.contains("vault-lane"));
    let full = s.activity_summary(Some(Since::Time(0))).unwrap();
    assert_eq!(full.live_holds, 2);
}

#[test]
fn webhooks_carry_only_matching_in_scope_events() {
    let (s, _) = store_with_clock();
    new_lane(&s, "shop-lane", "shop", "shopper");
    let created = s
        .create_webhook(
            NewWebhook {
                url: "https://hooks.example.test/another".into(),
                filters: vec![WebhookFilter::NeedsDecision, WebhookFilter::LaneStatus],
                project_globs: Some(vec!["/work/shop".into()]),
                description: Some("manager bot".into()),
            },
            None,
        )
        .unwrap();
    assert!(created.secret.starts_with("whsec_"));
    let hook = created.webhook.webhook_id.clone();

    // Events before the subscription are not delivered.
    assert_eq!(s.queue_webhook_deliveries().unwrap(), 0);

    new_lane(&s, "vault-lane", "vault", "vaulter");
    set_status(&s, "vault-lane", LaneStatus::NeedsDecision);
    set_status(&s, "shop-lane", LaneStatus::Working);
    set_status(&s, "shop-lane", LaneStatus::NeedsDecision);
    hold(&s, "shopper@shop", "shop", "a.rs"); // not subscribed
    assert_eq!(s.queue_webhook_deliveries().unwrap(), 2);
    assert_eq!(s.queue_webhook_deliveries().unwrap(), 0, "each event once");

    let due = s.claim_due_deliveries(10).unwrap();
    assert_eq!(due.len(), 2);
    let mut bodies: Vec<serde_json::Value> = due
        .iter()
        .map(|d| serde_json::from_str(&d.body).unwrap())
        .collect();
    // Queued in the same millisecond, the two can be claimed in either order.
    bodies.sort_by_key(|b| b["filter"] != "lane_status");
    assert_eq!(bodies[0]["filter"], "lane_status");
    assert_eq!(bodies[0]["data"]["to"], "working");
    assert_eq!(bodies[1]["filter"], "needs_decision");
    assert_eq!(bodies[1]["event"]["kind"], "lane.status_changed");
    for b in &bodies {
        assert!(!b.to_string().contains("vault"), "{b}");
        assert_eq!(b["webhook_id"], hook);
    }
    // Claimed deliveries are not handed out twice.
    assert!(s.claim_due_deliveries(10).unwrap().is_empty());

    // Signature: sha256=HMAC(secret, "<ts>.<body>").
    let h = due[0].headers(1_700_000_000_123);
    let sig = &h
        .iter()
        .find(|(k, _)| *k == "X-Another-Signature")
        .unwrap()
        .1;
    assert_eq!(
        sig,
        &webhook_signature(&created.secret, 1_700_000_000_123, &due[0].body)
    );
    assert_eq!(
        sig.trim_start_matches("sha256="),
        crypto::hmac_sha256_hex(
            created.secret.as_bytes(),
            format!("1700000000123.{}", due[0].body).as_bytes()
        )
    );

    let ok = s.record_delivery(&due[0].delivery_id, Ok(204)).unwrap();
    assert_eq!(ok.status, DeliveryStatus::Delivered);
    let list = s.webhooks(None).unwrap();
    assert_eq!((list[0].delivered, list[0].pending), (1, 1));
}

#[test]
fn failed_deliveries_back_off_then_die_and_can_be_retried() {
    let (s, clock) = store_with_clock();
    let c = s
        .create_webhook(
            NewWebhook {
                url: "http://127.0.0.1:9/hook".into(),
                filters: vec![WebhookFilter::All],
                project_globs: None,
                description: None,
            },
            None,
        )
        .unwrap();
    new_lane(&s, "l", "shop", "shopper");
    assert_eq!(s.queue_webhook_deliveries().unwrap(), 1);
    let expected_backoff = [10_000, 30_000, 120_000, 600_000, 3_600_000, 21_600_000];
    let mut id = String::new();
    for (attempt, wait) in expected_backoff.iter().enumerate() {
        let due = s.claim_due_deliveries(10).unwrap();
        assert_eq!(due.len(), 1, "attempt {}", attempt + 1);
        assert_eq!(due[0].attempt as usize, attempt + 1);
        id = due[0].delivery_id.clone();
        let failed = if attempt % 2 == 0 {
            Ok(500)
        } else {
            Err("connection refused".to_string())
        };
        let d = s.record_delivery(&id, failed).unwrap();
        assert_eq!(d.status, DeliveryStatus::Pending);
        advance(&clock, wait - 1);
        assert!(s.claim_due_deliveries(10).unwrap().is_empty());
        advance(&clock, 1);
    }
    let due = s.claim_due_deliveries(10).unwrap();
    assert_eq!(due[0].attempt, MAX_DELIVERY_ATTEMPTS);
    let dead = s.record_delivery(&id, Ok(503)).unwrap();
    assert_eq!(dead.status, DeliveryStatus::Dead);
    assert_eq!(dead.last_error.as_deref(), Some("HTTP 503"));
    let listed = s
        .webhook_deliveries(None, Some(DeliveryStatus::Dead), 10)
        .unwrap();
    assert_eq!(listed.len(), 1);

    let again = s.retry_delivery(&id).unwrap();
    assert_eq!(again.status, DeliveryStatus::Pending);
    assert_eq!(s.claim_due_deliveries(10).unwrap().len(), 1);
    assert_eq!(s.retry_delivery(&id).unwrap_err().code(), "conflict");

    // Removing the webhook drops what is pending.
    s.remove_webhook(&c.webhook.webhook_id, None).unwrap();
    assert!(s.webhooks(None).unwrap().is_empty());
    assert_eq!(
        s.remove_webhook(&c.webhook.webhook_id, None)
            .unwrap_err()
            .code(),
        "not_found"
    );
}

#[test]
fn remote_webhooks_stay_inside_the_token_scope() {
    let (s, _) = store_with_clock();
    let t = s
        .create_token("manager", &["/work/shop".into()], None, None)
        .unwrap();
    let scope = s.verify_token(&t.secret).unwrap();
    let other = s
        .create_token("manager", &["/work/vault".into()], None, None)
        .unwrap();
    let other_scope = s.verify_token(&other.secret).unwrap();
    let mk = |globs: Option<Vec<String>>, sc: &TokenScope| {
        s.create_webhook(
            NewWebhook {
                url: "https://hooks.example.test/x".into(),
                filters: vec![WebhookFilter::All],
                project_globs: globs,
                description: None,
            },
            Some(sc),
        )
    };
    // Defaults to the token's globs; widening is refused.
    let mine = mk(None, &scope).unwrap();
    assert_eq!(
        mine.webhook.project_globs,
        Some(vec!["/work/shop".to_string()])
    );
    assert_eq!(
        mk(Some(vec!["/work/**".into()]), &scope)
            .unwrap_err()
            .code(),
        "forbidden"
    );
    mk(None, &other_scope).unwrap();
    // Each token lists and removes only its own.
    assert_eq!(s.webhooks(Some(&scope)).unwrap().len(), 1);
    assert_eq!(s.webhooks(None).unwrap().len(), 2);
    let theirs = s.webhooks(Some(&other_scope)).unwrap()[0]
        .webhook_id
        .clone();
    // Someone else's webhook is indistinguishable from a missing one.
    let e = s.remove_webhook(&theirs, Some(&scope)).unwrap_err();
    let missing = s.remove_webhook("NOPE", Some(&scope)).unwrap_err();
    assert_eq!((e.code(), missing.code()), ("not_found", "not_found"));
    assert_eq!(
        e.to_string().replace(&theirs, "X"),
        missing.to_string().replace("NOPE", "X")
    );

    // A vault event never reaches the shop webhook, even with filter `all`.
    new_lane(&s, "vault-lane", "vault", "vaulter");
    new_lane(&s, "shop-lane", "shop", "shopper");
    s.queue_webhook_deliveries().unwrap();
    let shop = s
        .webhook_deliveries(Some(&mine.webhook.webhook_id), None, 50)
        .unwrap();
    assert_eq!(shop.len(), 1);
    assert_eq!(shop[0].kind, "lane.created");
    let body = &s.claim_due_deliveries(10).unwrap();
    assert!(body
        .iter()
        .filter(|d| d.webhook_id == mine.webhook.webhook_id)
        .all(|d| d.body.contains("shop-lane") && !d.body.contains("vault-lane")));

    // URLs: https, or http to loopback only.
    for bad in ["http://hooks.example.test/x", "file:///etc/passwd"] {
        let r = s.create_webhook(
            NewWebhook {
                url: bad.into(),
                filters: vec![WebhookFilter::All],
                project_globs: None,
                description: None,
            },
            None,
        );
        assert_eq!(r.unwrap_err().code(), "invalid", "{bad}");
    }
}

#[test]
fn hold_conflicts_and_expiring_leases_are_events() {
    let (s, clock) = store_with_clock();
    hold(&s, "shopper@shop", "shop", "src/cart.rs");
    let refused = hold(&s, "manager", "shop", "src/cart.rs");
    assert!(refused.granted.is_empty());
    let ev = s
        .events(50)
        .unwrap()
        .into_iter()
        .rfind(|e| e.kind == "hold.conflict")
        .expect("conflict event");
    assert_eq!(ev.payload["holders"][0], "shopper@shop");
    let feed = all_items(&s, None);
    assert!(feed
        .iter()
        .any(|i| i.summary.contains("was refused a hold on shop:src/cart.rs")));

    let lease = s
        .register_resource(dev_server("shopper@shop", Some(600)))
        .unwrap();
    let count = |s: &Store| {
        s.events(500)
            .unwrap()
            .iter()
            .filter(|e| e.kind == "resource.lease_expiring")
            .count()
    };
    s.sweep().unwrap();
    assert_eq!(count(&s), 0, "10 minutes left");
    advance(&clock, 6 * 60_000);
    s.sweep().unwrap();
    s.sweep().unwrap();
    assert_eq!(count(&s), 1, "once, under 5 minutes left");
    // Renewing re-arms the warning.
    advance(&clock, 1);
    s.heartbeat_resources("shopper@shop", &[]).unwrap();
    advance(&clock, 6 * 60_000);
    s.sweep().unwrap();
    assert_eq!(count(&s), 2);
    let ev = s
        .events(500)
        .unwrap()
        .into_iter()
        .rfind(|e| e.kind == "resource.lease_expiring")
        .unwrap();
    assert_eq!(ev.payload["resource_id"], lease.resource.resource_id);
    assert!(ev.payload["project_id"].is_string());
}

#[test]
fn upgrades_a_database_created_before_webhooks() {
    let dir = tempfile::tempdir().unwrap();
    {
        let old = Store::open_at_schema(dir.path(), 6).unwrap();
        old.ensure_project("t", "/work/shop").unwrap();
        old.register_agent(RegisterAgent {
            name: "a".into(),
            program: Program::Other,
            project: Some("/work/shop".into()),
            model: None,
            description: None,
        })
        .unwrap();
    }
    let s = Store::open(dir.path()).unwrap();
    assert!(Store::schema_version() >= 7);
    assert!(s.webhooks(None).unwrap().is_empty());
    s.register_resource(dev_server("a@shop", None)).unwrap();
    s.sweep().unwrap();
    s.create_webhook(
        NewWebhook {
            url: "https://hooks.example.test/x".into(),
            filters: vec![WebhookFilter::LeaseExpiring],
            project_globs: Some(vec!["shop".into()]),
            description: None,
        },
        None,
    )
    .unwrap();
}

fn shop_hook(s: &Store, scope: &TokenScope, filters: Vec<WebhookFilter>) -> WebhookCreated {
    s.create_webhook(
        NewWebhook {
            url: "https://hooks.example.test/x".into(),
            filters,
            project_globs: None,
            description: None,
        },
        Some(scope),
    )
    .unwrap()
}

#[test]
fn webhooks_stop_when_their_token_is_revoked_or_expires() {
    let (s, clock) = store_with_clock();
    let t = s
        .create_token("manager", &["/work/shop".into()], None, None)
        .unwrap();
    let scope = s.verify_token(&t.secret).unwrap();
    let hook = shop_hook(&s, &scope, vec![WebhookFilter::LaneStatus]);
    new_lane(&s, "before", "shop", "shopper");
    assert_eq!(s.queue_webhook_deliveries().unwrap(), 1);

    // Revoke, then emit: nothing new is queued and the pending delivery dies.
    s.revoke_token(&t.token.token_id).unwrap();
    new_lane(&s, "after", "shop", "shopper");
    assert_eq!(s.queue_webhook_deliveries().unwrap(), 0);
    assert!(s.claim_due_deliveries(10).unwrap().is_empty());
    let h = &s.webhooks(None).unwrap()[0];
    assert_eq!(h.webhook_id, hook.webhook.webhook_id);
    assert_eq!(h.disabled_reason.as_deref(), Some("token revoked"));
    assert_eq!((h.pending, h.dead), (0, 1));
    assert_eq!(
        s.queue_test_delivery(&h.webhook_id).unwrap_err().code(),
        "conflict"
    );
    // A dead delivery on a disabled webhook can't be retried: nothing would ever send it.
    let dead = s
        .webhook_deliveries(Some(&h.webhook_id), Some(DeliveryStatus::Dead), 10)
        .unwrap();
    assert_eq!(
        s.retry_delivery(&dead[0].delivery_id).unwrap_err().code(),
        "conflict"
    );
    let still_dead = s
        .webhook_deliveries(Some(&h.webhook_id), Some(DeliveryStatus::Dead), 10)
        .unwrap();
    assert_eq!(still_dead.len(), 1);

    // An expiring token: fine until it lapses, then disabled.
    let t2 = s
        .create_token("manager", &["/work/shop".into()], None, Some(3600))
        .unwrap();
    let scope2 = s.verify_token(&t2.secret).unwrap();
    shop_hook(&s, &scope2, vec![WebhookFilter::LaneStatus]);
    new_lane(&s, "soon", "shop", "shopper");
    assert_eq!(s.queue_webhook_deliveries().unwrap(), 1);
    advance(&clock, 3_600_001);
    new_lane(&s, "later", "shop", "shopper");
    assert_eq!(s.queue_webhook_deliveries().unwrap(), 0);
    assert!(s
        .webhooks(None)
        .unwrap()
        .iter()
        .any(|h| h.disabled_reason.as_deref() == Some("token expired")));
    // Local webhooks (no token) are unaffected.
    s.create_webhook(
        NewWebhook {
            url: "https://hooks.example.test/local".into(),
            filters: vec![WebhookFilter::LaneStatus],
            project_globs: Some(vec!["shop".into()]),
            description: None,
        },
        None,
    )
    .unwrap();
    new_lane(&s, "local", "shop", "shopper");
    assert_eq!(s.queue_webhook_deliveries().unwrap(), 1);
}

#[test]
fn webhooks_follow_the_own_agent_rule_for_project_less_events() {
    let (s, _) = store_with_clock();
    s.register_agent(RegisterAgent {
        name: "other".into(),
        program: Program::Other,
        project: None,
        model: None,
        description: None,
    })
    .unwrap();
    let t = s
        .create_token("manager", &["/work/shop".into()], None, None)
        .unwrap();
    let scope = s.verify_token(&t.secret).unwrap();
    let hook = shop_hook(&s, &scope, vec![WebhookFilter::All]);
    // Project-less: the token's own agent's session (delivered, as SSE would show it) and
    // another project-less agent's (not delivered).
    s.heartbeat("manager", false).unwrap();
    s.session("manager", SessionPhase::Start, SessionInfo::default())
        .unwrap();
    s.session("other", SessionPhase::Start, SessionInfo::default())
        .unwrap();
    s.queue_webhook_deliveries().unwrap();
    let kinds: Vec<(String, String)> = s
        .claim_due_deliveries(50)
        .unwrap()
        .into_iter()
        .filter(|d| d.webhook_id == hook.webhook.webhook_id)
        .map(|d| (d.kind, d.body))
        .collect();
    assert_eq!(kinds.len(), 1, "{kinds:?}");
    assert_eq!(kinds[0].0, "session.started");
    let manager_id = s.agent("manager").unwrap().agent_id;
    assert!(kinds[0].1.contains(&manager_id));
    // The SSE / poll feed agrees.
    let feed = s
        .activity_scoped(
            ActivityQuery {
                since: Some(Since::Time(0)),
                limit: 500,
                kinds: vec![ActivityKind::Session],
            },
            Some(&scope),
        )
        .unwrap();
    assert!(feed.items.iter().all(|i| i.actor != "other"));
    assert!(feed
        .items
        .iter()
        .any(|i| i.kind == "session.started" && i.actor == "manager"));
}

#[test]
fn object_events_follow_the_objects_project_only() {
    let (s, _) = store_with_clock();
    // A vault lane led by a shop agent stays a vault event.
    new_lane(&s, "vault-work", "vault", "shopper@shop");
    let t = s
        .create_token("manager", &["/work/shop".into()], None, None)
        .unwrap();
    let shop = s.verify_token(&t.secret).unwrap();
    let items = all_items(&s, Some(&shop));
    assert!(
        items.iter().all(|i| !i.summary.contains("vault-work")),
        "{items:?}"
    );
    // A shop hold by a vault agent is a shop event.
    hold(&s, "vaulter@vault", "shop", "src/x.rs");
    assert!(all_items(&s, Some(&shop))
        .iter()
        .any(|i| i.kind == "hold.acquired"));
}

#[test]
fn webhook_payloads_carry_ids_and_statuses_only() {
    let (s, _) = store_with_clock();
    let hook = s
        .create_webhook(
            NewWebhook {
                url: "https://example.test/h".into(),
                filters: vec![WebhookFilter::RequestAnswered, WebhookFilter::All],
                project_globs: None,
                description: None,
            },
            None,
        )
        .unwrap();
    let m = s
        .send_message(SendMessage {
            from: "manager".into(),
            to: vec!["shopper@shop".into()],
            subject: "please rotate the staging keys".into(),
            body: "do it".into(),
            request: Some(NewRequest::default()),
            ..Default::default()
        })
        .unwrap();
    let rid = m.request_id.clone().unwrap();
    s.update_request(RequestUpdate {
        agent: "shopper@shop".into(),
        request_id: rid,
        action: RequestAction::Complete,
        note: Some("the private completion note".into()),
        due_in_seconds: None,
    })
    .unwrap();
    // Free text in resource and reap events: purpose and reason.
    s.register_resource(RegisterResource {
        purpose: Some("the private purpose".into()),
        ..dev_server("shopper@shop", None)
    })
    .unwrap();
    s.note_reap_request(
        "shopper@shop",
        "dev_server:vite@5173",
        "the private reap reason",
    )
    .unwrap();
    s.queue_webhook_deliveries().unwrap();
    let bodies: Vec<String> = s
        .claim_due_deliveries(200)
        .unwrap()
        .into_iter()
        .filter(|d| d.webhook_id == hook.webhook.webhook_id)
        .map(|d| d.body)
        .collect();
    assert!(
        bodies.iter().any(|b| b.contains("request.completed")),
        "{bodies:?}"
    );
    assert!(
        bodies.iter().any(|b| b.contains("resource.reap_requested")),
        "{bodies:?}"
    );
    // The whole serialized delivery, summary included, carries no free text.
    for b in &bodies {
        for leak in [
            "private completion note",
            "private purpose",
            "private reap reason",
            "staging keys",
        ] {
            assert!(!b.contains(leak), "{leak} in {b}");
        }
    }
    // Nor does the feed a token reads, for resource purposes and reap reasons.
    let feed = serde_json::to_string(&all_items(&s, None)).unwrap();
    assert!(!feed.contains("private purpose") && !feed.contains("private reap reason"));
}

#[test]
fn renewal_in_the_warning_sweep_re_arms_the_expiry_warning() {
    let (s, clock) = store_with_clock();
    s.register_resource(dev_server("shopper@shop", Some(600)))
        .unwrap();
    let count = |s: &Store| {
        s.events(500)
            .unwrap()
            .iter()
            .filter(|e| e.kind == "resource.lease_expiring")
            .count()
    };
    // The heartbeat's own sweep warns, and the same transaction renews: no clock tick between.
    advance(&clock, 6 * 60_000);
    s.heartbeat_resources("shopper@shop", &[]).unwrap();
    assert_eq!(count(&s), 1);
    advance(&clock, 6 * 60_000);
    s.sweep().unwrap();
    assert_eq!(count(&s), 2, "the renewed lease warns again");
}

#[test]
fn project_less_requests_into_another_project_stay_hidden() {
    let (s, _) = store_with_clock();
    // A project-less manager asks a vault agent for something.
    s.send_message(SendMessage {
        from: "manager".into(),
        to: vec!["vaulter@vault".into()],
        subject: "rotate".into(),
        body: "please".into(),
        request: Some(NewRequest::default()),
        ..Default::default()
    })
    .unwrap();
    // The manager's own shop-scoped token doesn't see it: the request is vault's.
    let t = s
        .create_token("manager", &["/work/shop".into()], None, None)
        .unwrap();
    let shop = s.verify_token(&t.secret).unwrap();
    let sum = s
        .activity_summary_scoped(Some(Since::Time(0)), Some(&shop))
        .unwrap();
    assert_eq!(
        (sum.requests.open, sum.requests.claimed, sum.requests.stale),
        (0, 0, 0),
        "{sum:?}"
    );
    assert!(!sum.text.contains("rotate"), "{}", sum.text);
    // Locally it counts.
    assert_eq!(
        s.activity_summary(Some(Since::Time(0)))
            .unwrap()
            .requests
            .open,
        1
    );
}
