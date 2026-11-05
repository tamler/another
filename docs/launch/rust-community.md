# Rust community — preparation notes, not posts

Nothing here has been submitted. These are facts for a human to write from, not copy to paste, because the main venue refuses model-written articles. Read 2026-11-05; recheck on the day.

## This Week in Rust

[The README](https://github.com/rust-lang/this-week-in-rust) takes links through a pull request to `drafts/`, and says: "We expect community links to be articles that were created by humans. We don't accept articles written by LLMs."

- **Do not submit** the [Hashnode tutorial](hashnode.md), or anything else model-written, as a community link.
- **What would qualify:** a human-written how-to or deep dive with a Rust angle. It also needs no paywall and no email capture. A product announcement is a weak fit; the README asks for how-tos and deep dives, and does not address self-promotion.
- **Call for participation:** the README lists calls for participation in Rust open source projects. [good-first-issues.md](good-first-issues.md) could supply tasks once real issues exist. Check the section's current format first.

## Crate of the Week

[The nomination thread](https://users.rust-lang.org/t/crate-of-the-week/2704) asks for "useful, unsung and/or ubercool" crates. Its first post does not say whether self-nomination is allowed or whether a crate must be on crates.io.

**Another is not on crates.io** (checked 2026-11-05), so read recent nominations first. Without a published crate this is probably a poor fit. Publishing is a separate owner decision; see [mcp-directories.md](mcp-directories.md).

## r/rust

**Rules not verified.** The rules endpoint returned 403 to an unauthenticated request on 2026-11-05, so no eligibility is claimed. A human should read the current rules while signed in, and there is no draft here for that reason.

## Facts a human author can use

All from this repository. Check each against the release being discussed.

- A single Rust binary: CLI and MCP server. MSRV 1.88. Apache-2.0.
- Install: `cargo install --locked --git https://github.com/tamler/another another`.
- State is one local SQLite database, with no signup or product telemetry.
- Path holds are advisory: `another hold check` exits 3 on a conflict. They signal intent and do not lock files.
- It does not start or manage agents.

Do not claim benchmarks, adoption or production use. None is recorded in this packet. Disclose that the author is affiliated with the project.
