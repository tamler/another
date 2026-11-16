//! What the OAuth endpoints refuse, over HTTP: dynamic registration accepts only public
//! authorization-code clients; `/authorize` sends a bad `response_type` or `resource` back to
//! the registered redirect as an RFC 6749 error and opens no pending authorization; `/token`
//! refuses client secrets, a missing or foreign `resource`, and grant types it does not
//! support.

use std::net::SocketAddr;

use another::oauth_http::parse_public_url;
use another::serve::{http_router, HttpOptions};
use another_core::Store;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const REDIRECT: &str = "https://app.example/callback";

struct Server {
    store: Store,
    base: String,
    ct: CancellationToken,
    _dir: tempfile::TempDir,
}

async fn start() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let ct = CancellationToken::new();
    let opts = HttpOptions {
        auth: true,
        bound_port: addr.port(),
        oauth: Some(parse_public_url(&base).unwrap()),
        ..Default::default()
    };
    let app = http_router(store.clone(), ct.clone(), &opts);
    let stop = ct.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { stop.cancelled_owned().await })
        .await
        .unwrap();
    });
    Server {
        store,
        base,
        ct,
        _dir: dir,
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Status and JSON body of a registration request.
async fn register(srv: &Server, body: Value) -> (u16, Value) {
    let res = http()
        .post(format!("{}/oauth/register", srv.base))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = res.status().as_u16();
    (status, res.json().await.unwrap())
}

fn client(extra: Value) -> Value {
    let mut body = json!({
        "client_name": "App",
        "redirect_uris": [REDIRECT],
        "application_type": "web",
    });
    for (k, v) in extra.as_object().unwrap() {
        body[k] = v.clone();
    }
    body
}

#[tokio::test]
async fn registration_accepts_only_public_authorization_code_clients() {
    let srv = start().await;
    let refused = [
        (
            json!({"token_endpoint_auth_method": "client_secret_basic"}),
            "only public clients",
        ),
        (
            json!({"grant_types": ["authorization_code", "client_credentials"]}),
            "grant_types must be",
        ),
        (
            json!({"response_types": ["code", "token"]}),
            "response_types must be code",
        ),
    ];
    for (extra, message) in refused {
        let (status, body) = register(&srv, client(extra.clone())).await;
        assert_eq!(status, 400, "{extra}: {body}");
        assert_eq!(body["error"], "invalid_request", "{extra}: {body}");
        let text = body["error_description"].as_str().unwrap();
        assert!(text.contains(message), "{extra}: {text}");
        assert!(body["client_id"].is_null(), "{extra}: {body}");
    }

    let (status, body) = register(
        &srv,
        client(json!({
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
        })),
    )
    .await;
    assert!(status == 200 || status == 201, "{status}: {body}");
    assert!(body["client_id"].is_string(), "{body}");
    srv.ct.cancel();
}

/// Once the redirect is a registered one, `/authorize` errors go back to the client with
/// `error`, the client's `state` and `iss`, and no authorization is left waiting.
#[tokio::test]
async fn authorize_sends_bad_requests_back_to_the_registered_redirect() {
    let srv = start().await;
    let (_, registered) = register(&srv, client(json!({}))).await;
    let client_id = registered["client_id"].as_str().unwrap().to_string();
    let mcp = format!("{}/mcp", srv.base);
    let cases: [(&str, Option<&str>, &str); 4] = [
        ("token", Some(mcp.as_str()), "unsupported_response_type"),
        ("code", None, "invalid_request"),
        (
            "code",
            Some("https://elsewhere.example/mcp"),
            "invalid_target",
        ),
        ("code", Some("not a url"), "invalid_target"),
    ];
    for (response_type, resource, error) in cases {
        let mut pairs = vec![
            ("response_type", response_type),
            ("client_id", client_id.as_str()),
            ("redirect_uri", REDIRECT),
            ("code_challenge", CHALLENGE),
            ("code_challenge_method", "S256"),
            ("state", "st-1"),
        ];
        if let Some(r) = resource {
            pairs.push(("resource", r));
        }
        let res = http()
            .get(format!("{}/authorize?{}", srv.base, form(&pairs)))
            .send()
            .await
            .unwrap();
        let case = format!("{response_type} {resource:?}");
        assert_eq!(res.status(), 303, "{case}");
        let location = res.headers()["location"].to_str().unwrap().to_string();
        assert!(location.starts_with(REDIRECT), "{case}: {location}");
        assert!(
            location.contains(&format!("error={error}")),
            "{case}: {location}"
        );
        assert!(location.contains("state=st-1"), "{case}: {location}");
        assert!(location.contains("iss="), "{case}: {location}");
        assert!(!location.contains("code="), "{case}: {location}");
    }
    assert!(srv.store.oauth_pending().unwrap().is_empty());
    srv.ct.cancel();
}

#[tokio::test]
async fn token_refuses_secrets_foreign_resources_and_other_grants() {
    let srv = start().await;
    let mcp = format!("{}/mcp", srv.base);
    let post = |pairs: Vec<(&'static str, String)>| {
        let url = format!("{}/oauth/token", srv.base);
        async move {
            let body = pairs
                .iter()
                .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
                .collect::<Vec<_>>()
                .join("&");
            let res = http()
                .post(url)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(body)
                .send()
                .await
                .unwrap();
            let status = res.status().as_u16();
            let json: Value = res.json().await.unwrap();
            (status, json)
        }
    };
    let base = |grant: &str| {
        vec![
            ("grant_type", grant.to_string()),
            ("client_id", "app".to_string()),
            ("code", "c".to_string()),
            ("code_verifier", "v".to_string()),
            ("redirect_uri", REDIRECT.to_string()),
            ("resource", mcp.clone()),
        ]
    };

    for extra in [("client_secret", "s3cr3t"), ("client_assertion", "jwt")] {
        let mut pairs = base("authorization_code");
        pairs.push((extra.0, extra.1.to_string()));
        let (status, body) = post(pairs).await;
        assert_eq!(status, 401, "{extra:?}: {body}");
        assert_eq!(body["error"], "invalid_client", "{extra:?}: {body}");
    }

    let mut missing = base("authorization_code");
    missing.retain(|(k, _)| *k != "resource");
    let (status, body) = post(missing).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"], "invalid_request", "{body}");

    let mut foreign = base("authorization_code");
    foreign.retain(|(k, _)| *k != "resource");
    foreign.push(("resource", "https://elsewhere.example/mcp".to_string()));
    let (status, body) = post(foreign).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"], "invalid_target", "{body}");

    for grant in ["client_credentials", "password", ""] {
        let (status, body) = post(base(grant)).await;
        assert_eq!(status, 400, "{grant:?}: {body}");
        assert_eq!(body["error"], "invalid_request", "{grant:?}: {body}");
        assert!(
            body["error_description"]
                .as_str()
                .unwrap()
                .contains("grant_type must be"),
            "{grant:?}: {body}"
        );
    }
    srv.ct.cancel();
}
