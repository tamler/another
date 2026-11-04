# Examples

Four runnable scenarios. Each is a POSIX shell script that drives the real `another` binary and checks the results, so it doubles as a test. CI runs all four headless on every pull request.

None of them use a manager agent. Peers coordinate by themselves, and a person answers decisions and approvals directly.

| Example | What it shows |
|---|---|
| [peer-lanes](peer-lanes/) | Two peer agents split one repository with lanes and lane-aware holds, then resolve a cross-lane conflict by mail |
| [human-decision](human-decision/) | An agent asks a person a structured question, and the person answers over HTTP: from the machine, then "from the phone" with their own token |
| [spend-approval](spend-approval/) | A GPU budget with an approval gate: a lease starts only after the person approves, and a denied request never starts |
| [remote-invite](remote-invite/) | A remote agent joins a hub with a single-use invite, across two nodes on localhost, reports into a lane, and is kicked |

## Running them

You need `another` (a release binary, or `cargo build` in this checkout), plus `jq`, `curl` and `python3` (used to pick a free port).

```bash
sh examples/run-all.sh
```

To run one example:

```bash
sh examples/peer-lanes/run.sh
```

- Each script runs in a fresh temporary `ANOTHER_HOME` and project folder, so your `~/.another` is never touched. Everything is deleted when the script exits.
- It uses `another` from your `PATH`, else `target/release/another` or `target/debug/another` in this checkout. Set `ANOTHER_BIN` to choose a binary.
- Every command is echoed as `$ another …` with its output, so the transcript reads as a walkthrough.
- Each check prints `ok: …`. The script exits 1 at the first failed check.
