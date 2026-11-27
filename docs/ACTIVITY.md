# Activity feed

`another activity` shows event summaries, with no message bodies. Use `--summary`
for counts and highlights, `--kinds mail,request` to filter categories, and `--json`
for structured output.

```bash
another activity --since 3600
another activity --since 1h --kinds mail,request
another stats --since 3600
```

`--since` accepts a nonnegative duration in bare seconds (`3600` means one hour),
or with an `s`, `m`, `h`, `d` or `w` suffix. Zero means the current time. It also
accepts an RFC 3339 timestamp or an event id cursor. A timestamp includes events
at or after that time; a cursor includes events strictly after that event.
Use the feed's `next_since` value as the next cursor.

Activity summaries and stats use the same start-point syntax. The
[board](BOARD.md) accepts durations and timestamps, but refuses event ids.
Invalid start points return an error.
