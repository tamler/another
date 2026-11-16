//! `trial_stats` counts every wake but lists at most 50 of them, newest first, so a busy
//! trial cannot make the report unbounded. The counts must stay exact past the cap.

use another_core::*;

#[test]
fn wakes_are_counted_in_full_but_listed_fifty_newest_first() {
    let s = Store::open_in_memory(Clock::manual(1_800_000_000_000).0).unwrap();
    s.register_agent(RegisterAgent {
        name: "alice".into(),
        program: Program::Claude,
        project: None,
        model: None,
        description: None,
    })
    .unwrap();
    for n in 0..60 {
        let outcome = if n % 10 == 0 { "failed" } else { "sent" };
        s.record_wake(
            "alice",
            WakeVia::Herdr,
            &format!("w{n}:p1"),
            outcome,
            "",
            &[],
        )
        .unwrap();
    }

    let stats = s.trial_stats(None).unwrap();
    assert_eq!(stats.wakes_sent, 54);
    assert_eq!(stats.wakes_failed, 6);
    assert_eq!(stats.wakes.len(), 50, "the list is capped");
    let targets: Vec<&str> = stats.wakes.iter().map(|w| w.target.as_str()).collect();
    assert_eq!(targets.first(), Some(&"w59:p1"), "newest first");
    assert_eq!(
        targets.last(),
        Some(&"w10:p1"),
        "the oldest ten are left out"
    );
}
