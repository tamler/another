# Assets

Images used by the repository README and, for the two diagrams, by toanother.one. Nothing here is model-generated: the cards are drawn in code from the site's mark and font outlines, the diagrams are hand-written SVG, and the terminal demo is a recording of real commands.

| File | What it is | How it was made |
|---|---|---|
| `banner.png` | 1600x400 README header: the mark, the wordmark and the coordination message | `python3 docs/assets/render-assets.py` (needs `fonttools`). Reuses `site/render-brand.py`: palette, mark, JetBrains Mono ExtraBold outlines |
| `social-preview.png` | 1280x640 GitHub social preview, same lockup as the site's `og.png` | Same script. Set it under the repository's Settings, Social preview, after merge; the API does not expose that field |
| `path-holds.svg` | Diagram: a client edit to a held file is stopped by the edit hook and names the holder; a shell edit is not; the agent sends a release request instead | Hand-written SVG, site palette, monospace text |
| `handoff.svg` | Diagram: a `--request` message opens a request; ack records receipt only; open, claimed, done, release, stale | Hand-written SVG |
| `mid-task-notice.svg` | Diagram: mail that lands mid-task is announced by the post-tool hook after the next tool call; an idle Herdr agent is woken by the daemon; each message is announced once | Hand-written SVG |
| `demo.gif` | Terminal recording of the Alice and Bob handoff plus a hold conflict, 24 seconds | `asciinema rec` of `demo.cast` below, rendered with `agg` (Menlo 15px, 118 columns, terminal palette from `site/style.css`) |
| `demo.cast` | The asciinema v3 recording the GIF was rendered from | Recorded 2026-11-12 against a worktree build of `main` with a throwaway `HOME` and `ANOTHER_HOME`, in a temp project named `shop`. No real `~/.another` data, no secrets; the only path in it is `sh ../demo.sh` |

The recording script was: register `alice` and `bob`, `another send ... --request`, `another inbox`, `another hold acquire`, `another hold check` (exit 3), `another reply`, `another ack`, `another holds`. Every line of output is what the binary printed. Ids and timestamps will differ when you run it.

Palette: paper `#f8f8f4`, ink `#292922`, ochre `#705000` (and `#e4c56e` for the highlight), terminal `#25251f` on `#f4f2e7`. The diagrams declare `Fragment Mono, Menlo, Consolas, Liberation Mono, monospace`; on the site the first face is self-hosted, elsewhere a system monospace is used, so their text boxes are sized for the wider fallback.

JetBrains Mono is under the SIL Open Font License (see `site/fonts/jetbrains/OFL.txt`). The PNGs embed its outlines as pixels, which the OFL permits.

Regenerate the cards after a palette or wording change with the render script. Re-record the demo if the CLI output changes, from a temp home, and look at the frames before committing.
