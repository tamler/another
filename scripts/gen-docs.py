#!/usr/bin/env python3
"""Generate the command and tool references, and check that everything is documented.

    scripts/gen-docs.py [--check] [--bin PATH]

Writes docs/CLI.md (every subcommand's --help) and docs/MCP.md (every MCP tool, from the
server's own tools/list). With --check nothing is written; it fails when:

- a CLI subcommand or an MCP tool has no description;
- docs/CLI.md or docs/MCP.md is not what the binary generates now (run without --check);
- an HTTP route in the routers (serve.rs, fed.rs) is missing from docs/HTTP.md.

--bin defaults to $ANOTHER_BIN, else target/debug/another.
"""
import json
import os
import re
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CLI_MD = os.path.join(ROOT, "docs", "CLI.md")
MCP_MD = os.path.join(ROOT, "docs", "MCP.md")
HTTP_MD = os.path.join(ROOT, "docs", "HTTP.md")
ROUTERS = [
    os.path.join(ROOT, "crates", "another", "src", "serve.rs"),
    os.path.join(ROOT, "crates", "another", "src", "fed.rs"),
]


def clean_env(home):
    """Help text must not depend on the caller: no ANOTHER_* values, a fixed width."""
    env = {k: v for k, v in os.environ.items() if not k.startswith("ANOTHER_")}
    env.update({"COLUMNS": "100", "NO_COLOR": "1", "ANOTHER_HOME": home, "HOME": home})
    return env


def help_text(binary, path, env):
    out = subprocess.run([binary, *path, "--help"], capture_output=True, text=True, env=env)
    if out.returncode != 0:
        sys.exit(f"`another {' '.join(path)} --help` failed: {out.stderr}")
    # The --home default names the temporary HOME; show the documented default instead.
    return out.stdout.replace(env["HOME"], "~").rstrip() + "\n"


def subcommands(text):
    """(name, about) pairs from a clap `Commands:` section."""
    m = re.search(r"^Commands:\n(.*?)(?:\n\n|\Z)", text, re.S | re.M)
    if not m:
        return []
    out, last = [], None
    for line in m.group(1).split("\n"):
        mm = re.match(r"^  (\S+)\s{2,}(.*)$", line)
        if mm:
            last = [mm.group(1), mm.group(2).strip()]
            out.append(last)
        elif re.match(r"^  (\S+)$", line):
            last = [line.strip(), ""]
            out.append(last)
        elif last is not None and line.startswith("      "):
            last[1] = (last[1] + " " + line.strip()).strip()
    return [(n, a) for n, a in out if n != "help"]


def walk(binary, env):
    """Every command path, depth first, with its about line and full --help."""
    rows = []

    def visit(path, about):
        text = help_text(binary, path, env)
        rows.append((path, about, text))
        for name, sub_about in subcommands(text):
            visit(path + [name], sub_about)

    top = help_text(binary, [], env)
    rows.append(([], top.split("\n", 1)[0], top))
    for name, about in subcommands(top):
        visit([name], about)
    return rows


def cli_markdown(rows):
    out = [
        "# CLI reference",
        "",
        "Generated from `another … --help` by `scripts/gen-docs.py`; do not edit by hand. "
        "CI fails when this file is out of date or a command has no description.",
        "",
        "| Command | What it does |",
        "|---|---|",
    ]
    for path, about, _ in rows[1:]:
        anchor = "another-" + "-".join(path)
        cell = about.replace("|", "\\|")
        out.append(f"| [`another {' '.join(path)}`](#{anchor}) | {cell} |")
    for path, _, text in rows:
        title = "another " + " ".join(path) if path else "another"
        out += ["", f"## {title}", "", "```text", text.rstrip("\n"), "```"]
    return "\n".join(out) + "\n"


def mcp_tools(binary, env):
    """tools/list from `another serve --stdio` (newline-delimited JSON-RPC)."""
    msgs = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "gen-docs", "version": "0"}}},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}},
    ]
    subprocess.run([binary, "init"], capture_output=True, env=env, check=True)
    proc = subprocess.Popen([binary, "serve", "--stdio"], stdin=subprocess.PIPE,
                            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, env=env)
    tools = None
    try:
        for m in msgs:
            proc.stdin.write(json.dumps(m) + "\n")
            proc.stdin.flush()
            if "id" not in m:
                continue
            while True:
                line = proc.stdout.readline()
                if not line:
                    sys.exit("MCP server closed the connection")
                reply = json.loads(line)
                if reply.get("id") == m["id"]:
                    break
            if m["id"] == 2:
                tools = reply["result"]["tools"]
    finally:
        proc.kill()
        proc.wait()
    return sorted(tools, key=lambda t: t["name"])


def schema_rows(schema):
    props = schema.get("properties", {}) or {}
    required = set(schema.get("required", []) or [])
    rows = []
    for name in sorted(props):
        p = props[name]
        kind = p.get("type")
        if isinstance(kind, list):
            kind = " or ".join(k for k in kind if k != "null") + ("" if "null" not in kind else ", optional")
        kind = kind or ("object" if "$ref" in p or "allOf" in p or "anyOf" in p else "any")
        desc = " ".join((p.get("description") or "").split()).replace("|", "\\|")
        rows.append(f"| `{name}` | {kind} | {'yes' if name in required else ''} | {desc} |")
    return rows


def mcp_markdown(tools):
    out = [
        "# MCP tools",
        "",
        "Generated from the server's own `tools/list` by `scripts/gen-docs.py`; do not edit by "
        "hand. CI fails when this file is out of date or a tool has no description. Remote "
        "(token) callers can use a subset; see [REMOTE.md](REMOTE.md).",
        "",
        "| Tool | What it does |",
        "|---|---|",
    ]
    for t in tools:
        first = " ".join(t.get("description", "").split()).split(". ")[0].rstrip(".")
        cell = first.replace("|", "\\|")
        out.append(f"| [`{t['name']}`](#{t['name']}) | {cell} |")
    for t in tools:
        out += ["", f"## {t['name']}", "", " ".join(t.get("description", "").split())]
        rows = schema_rows(t.get("inputSchema", {}))
        if rows:
            out += ["", "| Argument | Type | Required | Description |", "|---|---|---|---|", *rows]
    return "\n".join(out) + "\n"


def routes():
    found = set()
    for f in ROUTERS:
        s = open(f, encoding="utf-8").read()
        for m in re.finditer(r'\.route\(\s*"([^"]+)",\s*(?:axum::routing::)?(get|post|put|delete)\(', s):
            found.add((m.group(2).upper(), m.group(1)))
        for m in re.finditer(r'nest_service\("([^"]+)"', s):
            found.add(("POST", m.group(1)))
    return sorted(found, key=lambda r: (r[1], r[0]))


def main():
    args = sys.argv[1:]
    check = "--check" in args
    binary = os.environ.get("ANOTHER_BIN") or os.path.join(ROOT, "target", "debug", "another")
    if "--bin" in args:
        binary = args[args.index("--bin") + 1]
    problems = []
    with tempfile.TemporaryDirectory() as home:
        env = clean_env(home)
        rows = walk(binary, env)
        tools = mcp_tools(binary, env)
    for path, about, _ in rows[1:]:
        if not about.strip():
            problems.append(f"CLI: `another {' '.join(path)}` has no description (add a doc comment)")
    for t in tools:
        if not t.get("description", "").strip():
            problems.append(f"MCP: tool `{t['name']}` has no description")
    cli, mcp = cli_markdown(rows), mcp_markdown(tools)
    http = open(HTTP_MD, encoding="utf-8").read() if os.path.exists(HTTP_MD) else ""
    for method, path in routes():
        if not re.search(rf"`{re.escape(method)} {re.escape(path)}`", http):
            problems.append(f"HTTP: `{method} {path}` is not documented in docs/HTTP.md")
    for target, text in ((CLI_MD, cli), (MCP_MD, mcp)):
        current = open(target, encoding="utf-8").read() if os.path.exists(target) else ""
        if current != text:
            if check:
                problems.append(f"{os.path.relpath(target, ROOT)} is out of date: run scripts/gen-docs.py")
            else:
                open(target, "w", encoding="utf-8").write(text)
                print(f"wrote {os.path.relpath(target, ROOT)}")
    if problems:
        print("\n".join(problems), file=sys.stderr)
        sys.exit(1)
    print(f"docs ok: {len(rows) - 1} commands, {len(tools)} MCP tools, {len(routes())} HTTP routes")


if __name__ == "__main__":
    main()
