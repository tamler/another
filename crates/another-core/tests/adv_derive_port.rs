//! Adversarial (main): `derive_port`, which gives a resource its port when none is passed.

use another_core::derive_port;

#[test]
fn control_documented_forms_still_derive() {
    assert_eq!(derive_port("ollama@11434"), Some(11434));
    assert_eq!(derive_port("http://127.0.0.1:5173/app"), Some(5173));
    assert_eq!(derive_port("http://[::1]:3000"), Some(3000));
    assert_eq!(derive_port("postgres://u:pw@db:5432/app"), Some(5432));
}

#[test]
fn gap_a_url_with_a_query_or_fragment_and_no_path_keeps_its_port() {
    assert_eq!(derive_port("http://127.0.0.1:5173?debug=1"), Some(5173));
    assert_eq!(derive_port("http://127.0.0.1:5173#top"), Some(5173));
}

#[test]
fn gap_a_bare_ipv6_address_is_not_read_as_a_port() {
    for endpoint in ["::1", "fe80::8080", "[::1]", "http://[::1]"] {
        assert_eq!(derive_port(endpoint), None, "{endpoint}");
    }
}

/// Forms around the two fixes that must keep their meaning.
#[test]
fn neighbours_of_the_fixes_keep_their_meaning() {
    // Not IPv6, though it has several colons: the documented `tcp:host:port` form.
    assert_eq!(derive_port("tcp:0.0.0.0:5432"), Some(5432));
    // A zone id does not make a bare IPv6 address carry a port.
    assert_eq!(derive_port("fe80::1%en0"), None);
    // Bracketed IPv6 with a port, followed by a query.
    assert_eq!(derive_port("http://[::1]:3000?x=1"), Some(3000));
    // A path, query and fragment after the port.
    assert_eq!(derive_port("http://127.0.0.1:5173/app?x=1#y"), Some(5173));
    // No port at all.
    assert_eq!(derive_port("http://localhost?x=1"), None);
    // Userinfo that looks like a port is not one.
    assert_eq!(derive_port("postgres://user:1234@db"), None);
}
