//! Runs every fuzz harness on stable with deterministic pseudo-random inputs, so the
//! properties the fuzzers check are also checked by every `cargo test`. The libFuzzer runs
//! in `fuzz/` explore far more inputs; this keeps the harnesses compiling and passing.

/// xorshift64*: deterministic, no dependency, good enough to spread bytes.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn bytes(&mut self, max: usize) -> Vec<u8> {
        let len = (self.next() as usize) % (max + 1);
        (0..len).map(|_| self.next() as u8).collect()
    }
}

fn run(name: &str, seed: u64, iterations: usize, max_len: usize, f: fn(&[u8])) {
    let mut rng = Rng(seed);
    // The empty input and an all-0xff input first: both are complete recipes.
    f(&[]);
    f(&[0xff; 64]);
    for i in 0..iterations {
        let input = rng.bytes(max_len);
        let result = std::panic::catch_unwind(|| f(&input));
        if let Err(e) = result {
            let msg = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            panic!("{name}: input #{i} {input:?} failed: {msg}");
        }
    }
}

#[test]
fn ingest_events_smoke() {
    run(
        "ingest_events",
        0x5eed_0001,
        300,
        256,
        another_fuzz::ingest_events,
    );
}

#[test]
fn ingest_raw_smoke() {
    // Valid shapes first, then random bytes.
    for body in [
        r#"{"events":[]}"#,
        r#"{"events":[{"seq":1,"event_id":"e","node_id":"01FZZSPQKEN0DE000000000000","ts":0,"actor":"a","kind":"message.sent","payload":"{}","prev_id":null,"sig":"x"}]}"#,
        r#"{"events":[{"seq":-1,"event_id":"","node_id":"","ts":9223372036854775807,"actor":"","kind":"","payload":"\u0000","prev_id":"p","sig":""}]}"#,
    ] {
        another_fuzz::ingest_raw(body.as_bytes());
    }
    run(
        "ingest_raw",
        0x5eed_0002,
        200,
        256,
        another_fuzz::ingest_raw,
    );
}

#[test]
fn peer_request_smoke() {
    run(
        "peer_request",
        0x5eed_0003,
        300,
        128,
        another_fuzz::peer_request,
    );
}

#[test]
fn tool_args_smoke() {
    run("tool_args", 0x5eed_0004, 300, 256, another_fuzz::tool_args);
}

/// The nightly run's crash input (run 43891582711): a `text` argument that main's audit line
/// keeps, because note redaction (D70) is not on main. The oracle checks only what main
/// redacts; this input must pass.
#[test]
fn tool_args_nightly_crash_e60e2bc6_is_not_a_failure_on_main() {
    another_fuzz::tool_args(&[114, 43, 101, 7, 41, 27, 115, 117]);
}

#[test]
fn cli_args_smoke() {
    run("cli_args", 0x5eed_0005, 2000, 64, another_fuzz::cli_args);
}
