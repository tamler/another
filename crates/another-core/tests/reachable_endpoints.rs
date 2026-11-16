//! FINDINGS-main-reachable: `reachable` decides whether a foreign resource is listed as
//! reachable and whether it may be borrowed. It cuts the endpoint's authority only at `/`,
//! so a query or fragment straight after the port leaves "5173?debug=1" as the port and a
//! live resource reads as unreachable. `derive_port` accepts these spellings (#182).

use another_core::reachable;

#[test]
fn a_live_endpoint_with_a_query_or_fragment_is_reachable() {
    let v4 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let p = v4.local_addr().unwrap().port();
    for e in [
        format!("http://127.0.0.1:{p}?debug=1"),
        format!("http://127.0.0.1:{p}#top"),
    ] {
        assert!(reachable(Some(&e), None), "{e} is live");
        assert!(reachable(Some(&e), Some(p)), "{e} with its port is live");
    }
    // A `user@` prefix is not part of the host.
    let e = format!("http://user@127.0.0.1:{p}/");
    assert!(reachable(Some(&e), None), "{e} is live");
    // Controls that pass on main.
    assert!(reachable(Some(&format!("http://127.0.0.1:{p}/x?y")), None));
    drop(v4);
    assert!(!reachable(
        Some(&format!("http://127.0.0.1:{p}?debug=1")),
        None
    ));
}

#[test]
fn a_live_ipv6_endpoint_with_a_query_is_reachable() {
    let Ok(v6) = std::net::TcpListener::bind("[::1]:0") else {
        return; // no IPv6 loopback on this runner
    };
    let p = v6.local_addr().unwrap().port();
    assert!(reachable(Some(&format!("http://[::1]:{p}/")), None));
    let e = format!("http://[::1]:{p}?q=1");
    assert!(reachable(Some(&e), None), "{e} is live");
}

/// The authority is cut at `/`, `?` or `#` before a `user@` prefix is dropped, so an `@`
/// in the path, query or fragment is not read as userinfo: the probe goes to the live
/// host, not to a host named after it.
#[test]
fn an_at_sign_after_the_authority_is_not_userinfo() {
    let v4 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let p = v4.local_addr().unwrap().port();
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    for e in [
        format!("http://127.0.0.1:{p}/?mail=a@127.0.0.1:{dead}"),
        format!("http://127.0.0.1:{p}?mail=a@127.0.0.1:{dead}"),
        format!("http://127.0.0.1:{p}/x@127.0.0.1:{dead}"),
        format!("http://127.0.0.1:{p}#a@127.0.0.1:{dead}"),
    ] {
        assert!(reachable(Some(&e), None), "{e} is live");
    }
}
