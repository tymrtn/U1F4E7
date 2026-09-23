#!/usr/bin/env bash
# ci/check-cursor-plugin.sh — marketplace packaging checklist for the
# repo-root Cursor Plugin (single-plugin layout).
#
# Checks the items Cursor's plugin submission review expects us to keep
# valid in-tree: manifest JSON, kebab-case name, relative paths, mcp.json
# stdio command, skill frontmatter, and README usage. Does not submit
# anything to the marketplace.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

fail() {
    echo "ERROR: $*" >&2
    exit 1
}

python3 - "$repo_root" <<'PY'
import json
import re
import sys
from pathlib import Path

root = Path(sys.argv[1])
errors: list[str] = []

def err(msg: str) -> None:
    errors.append(msg)

manifest_path = root / ".cursor-plugin" / "plugin.json"
if not manifest_path.is_file():
    err("missing .cursor-plugin/plugin.json")
    print("\n".join(f"- {e}" for e in errors), file=sys.stderr)
    sys.exit(1)

try:
    manifest = json.loads(manifest_path.read_text())
except json.JSONDecodeError as exc:
    err(f"plugin.json is not valid JSON: {exc}")
    print("\n".join(f"- {e}" for e in errors), file=sys.stderr)
    sys.exit(1)

name = manifest.get("name")
if not isinstance(name, str) or not re.fullmatch(r"[a-z0-9][a-z0-9.-]*[a-z0-9]", name):
    err("plugin.json name must be lowercase kebab-case (start/end alphanumeric)")

if not manifest.get("description"):
    err("plugin.json description is required for marketplace listing")

def check_rel(label: str, raw: object) -> None:
    values = raw if isinstance(raw, list) else [raw]
    for value in values:
        if not isinstance(value, str):
            continue
        if value.startswith("/") or value.startswith("..") or ".." in Path(value).parts:
            err(f"{label} must be a relative path without '..': {value!r}")
            continue
        target = root / value
        if not target.exists():
            err(f"{label} path does not exist: {value}")

for field in ("logo", "skills", "mcpServers"):
    if field in manifest and isinstance(manifest[field], (str, list)):
        check_rel(field, manifest[field])

mcp_path = root / "mcp.json"
if not mcp_path.is_file():
    err("missing mcp.json")
else:
    try:
        mcp = json.loads(mcp_path.read_text())
    except json.JSONDecodeError as exc:
        err(f"mcp.json is not valid JSON: {exc}")
        mcp = None
    if isinstance(mcp, dict):
        servers = mcp.get("mcpServers")
        if not isinstance(servers, dict) or "envelope" not in servers:
            err("mcp.json must define mcpServers.envelope")
        else:
            server = servers["envelope"]
            if server.get("command") != "envelope":
                err('mcp.json command must be "envelope"')
            if server.get("args") != ["mcp"]:
                err('mcp.json args must be ["mcp"]')
            env = server.get("env") or {}
            token = env.get("ENVELOPE_AGENT_TOKEN")
            if token != "${ENVELOPE_AGENT_TOKEN}":
                err("mcp.json must pass ENVELOPE_AGENT_TOKEN only as ${ENVELOPE_AGENT_TOKEN}")
            leaked = json.dumps(mcp)
            if "env-agent-" in leaked or "sk-" in leaked:
                err("mcp.json must not contain secrets")

skill_path = root / "skills" / "envelope" / "SKILL.md"
if not skill_path.is_file():
    err("missing skills/envelope/SKILL.md")
else:
    text = skill_path.read_text()
    if not text.startswith("---"):
        err("skills/envelope/SKILL.md must start with YAML frontmatter")
    else:
        parts = text.split("---", 2)
        front = parts[1] if len(parts) >= 3 else ""
        if not re.search(r"(?m)^name:\s*envelope\s*$", front):
            err("skill frontmatter name must be envelope (match folder)")
        if not re.search(r"(?m)^description:\s*.+", front):
            err("skill frontmatter must include description")

readme = (root / "README.md").read_text()
if "Cursor Marketplace plugin" not in readme:
    err("README.md must document Cursor Marketplace plugin usage")
if "brew install tymrtn/u1f4e7/u1f4e7" not in readme:
    err("README.md must document the brew/PATH prerequisite")

marketplace = root / "MARKETPLACE.md"
if not marketplace.is_file():
    err("missing MARKETPLACE.md listing copy")
else:
    listing = marketplace.read_text()
    if "Inbox" not in listing:
        err("MARKETPLACE.md must name the Inbox category")
    if "cursor.com/marketplace/publish" not in listing:
        err("MARKETPLACE.md must point at the human publish URL")

variables = manifest.get("variables")
if not isinstance(variables, dict) or "ENVELOPE_AGENT_TOKEN" not in (
    (variables.get("properties") or {}) if isinstance(variables.get("properties"), dict) else {}
):
    err("plugin.json must declare ENVELOPE_AGENT_TOKEN under variables.properties")

if errors:
    print("Cursor plugin packaging checklist failed:", file=sys.stderr)
    print("\n".join(f"- {e}" for e in errors), file=sys.stderr)
    sys.exit(1)

print("OK: Cursor plugin packaging checklist passed.")
PY
