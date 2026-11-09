//! The session digest's memory use doesn't grow with the unread backlog. It lives in its own test
//! binary because it replaces the allocator and measures the whole process; this file must hold
//! only this test.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use another_core::{Clock, Importance, Program, RegisterAgent, SendMessage, Store};

struct Tracking;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(size: usize) {
    let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

// SAFETY: every call forwards to `System` with the caller's arguments; the counters don't touch
// the allocation.
unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { System.dealloc(p, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, layout, new_size) };
        if !q.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            grew(new_size);
        }
        q
    }
}

#[global_allocator]
static ALLOCATOR: Tracking = Tracking;

#[test]
fn a_large_urgent_backlog_does_not_load_every_body_into_the_digest() {
    const BACKLOG: usize = 64;
    const BODY: usize = 128 * 1024;

    let (clock, h) = Clock::manual(1_795_219_200_000);
    let store = Store::open_in_memory(clock).unwrap();
    store.ensure_project("t", "/work/shop").unwrap();
    for (name, program) in [("claude", Program::Claude), ("codex", Program::Codex)] {
        store
            .register_agent_trusted(RegisterAgent {
                name: name.into(),
                program,
                project: Some("/work/shop".into()),
                model: None,
                description: None,
            })
            .unwrap();
    }
    for i in 0..BACKLOG {
        h.fetch_add(60_000, Ordering::SeqCst);
        store
            .send_message(SendMessage {
                from: "codex@shop".into(),
                to: vec!["claude@shop".into()],
                subject: format!("m{i}"),
                body: "x".repeat(BODY),
                importance: Importance::Urgent,
                ..Default::default()
            })
            .unwrap();
    }

    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let digest = another::hooks::session_digest(&store, "claude@shop").unwrap();
    let used = PEAK.load(Ordering::Relaxed) - before;

    assert!(
        digest.contains(&format!("{BACKLOG} unread ({BACKLOG} urgent)")),
        "{digest}"
    );
    assert_eq!(digest.matches("[urgent] from").count(), 5, "{digest}");
    // Five listed messages plus the summary's eight newest events, each carrying its body, and
    // room for the row being read. A message sent within the hour costs both paths.
    assert!(
        used < 20 * BODY,
        "the digest lists five messages but held {used} bytes at its peak, with {BACKLOG} bodies of {BODY} bytes unread"
    );
}
