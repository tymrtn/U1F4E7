#!/usr/bin/env bash
# ci/check-agent-plugins.sh — packaging checklist for the agent plugins this
# repo ships from one root: Claude Code (.claude-plugin), Codex
# (.codex-plugin + .agents/plugins), and Cursor (.cursor-plugin + mcp.json),
# all sharing skills/. Every manifest version must equal the Cargo workspace
# version, so plugins move with releases. Checks files only; publishes nothing.

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
if "brew install tymrtn/u1f4e7/envelope" not in readme:
    err("README.md must document the brew/PATH prerequisite")
if "tymrtn/u1f4e7/u1f4e7" not in readme:
    err("README.md must note the u1f4e7 brew compat alias")

marketplace = root / "MARKETPLACE.md"
if not marketplace.is_file():
    err("missing MARKETPLACE.md listing copy")
else:
    listing = marketplace.read_text()
    if "Inbox" not in listing:
        err("MARKETPLACE.md must name the Inbox category")
    if "cursor.com/marketplace/publish" not in listing:
        err("MARKETPLACE.md must point at the human publish URL")
    if "brew install tymrtn/u1f4e7/envelope" not in listing:
        err("MARKETPLACE.md must prefer brew install tymrtn/u1f4e7/envelope")

variables = manifest.get("variables")
if not isinstance(variables, dict) or "ENVELOPE_AGENT_TOKEN" not in (
    (variables.get("properties") or {}) if isinstance(variables.get("properties"), dict) else {}
):
    err("plugin.json must declare ENVELOPE_AGENT_TOKEN under variables.properties")

def load_json(rel: str):
    path = root / rel
    if not path.is_file():
        err(f"missing {rel}")
        return None
    try:
        return json.loads(path.read_text())
    except json.JSONDecodeError as exc:
        err(f"{rel} is not valid JSON: {exc}")
        return None

cargo = (root / "Cargo.toml").read_text()
m = re.search(r'(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"', cargo)
cargo_version = m.group(1) if m else None
if not cargo_version:
    err("could not read [workspace.package] version from Cargo.toml")

def check_version(label: str, doc) -> None:
    if isinstance(doc, dict) and cargo_version and doc.get("version") != cargo_version:
        err(f"{label} version {doc.get('version')!r} must equal Cargo version {cargo_version!r}")

def check_skill(folder: str) -> None:
    path = root / "skills" / folder / "SKILL.md"
    if not path.is_file():
        err(f"missing skills/{folder}/SKILL.md")
        return
    text = path.read_text()
    parts = text.split("---", 2)
    front = parts[1] if text.startswith("---") and len(parts) >= 3 else ""
    if not re.search(rf"(?m)^name:\s*{re.escape(folder)}\s*$", front):
        err(f"skills/{folder}/SKILL.md frontmatter name must be {folder}")
    if not re.search(r"(?m)^description:\s*\S", front):
        err(f"skills/{folder}/SKILL.md frontmatter must include description")

check_version(".cursor-plugin/plugin.json", manifest)
check_skill("envelope-setup")

# Claude Code: token only through sensitive userConfig.
claude = load_json(".claude-plugin/plugin.json")
check_version(".claude-plugin/plugin.json", claude)
if isinstance(claude, dict):
    if claude.get("name") != "envelope":
        err('.claude-plugin/plugin.json name must be "envelope"')
    if claude.get("skills") != ["./skills/"]:
        err('.claude-plugin/plugin.json skills must be ["./skills/"]')
    server = ((claude.get("mcpServers") or {}).get("envelope")) or {}
    if server.get("command") != "envelope" or server.get("args") != ["mcp"]:
        err(".claude-plugin/plugin.json mcpServers.envelope must run `envelope mcp`")
    if (server.get("env") or {}).get("ENVELOPE_AGENT_TOKEN") != "${user_config.agent_token}":
        err(".claude-plugin/plugin.json must pass the token only as ${user_config.agent_token}")
    token_cfg = ((claude.get("userConfig") or {}).get("agent_token")) or {}
    if token_cfg.get("sensitive") is not True or token_cfg.get("required") is not True:
        err(".claude-plugin/plugin.json userConfig.agent_token must be sensitive and required")

claude_mkt = load_json(".claude-plugin/marketplace.json")
if isinstance(claude_mkt, dict):
    listed = claude_mkt.get("plugins")
    entries = [p for p in listed if isinstance(p, dict)] if isinstance(listed, list) else []
    if claude_mkt.get("name") != "envelope" or not claude_mkt.get("description"):
        err('.claude-plugin/marketplace.json needs name "envelope" and a description')
    if not any(p.get("name") == "envelope" and p.get("source") == "./" for p in entries):
        err('.claude-plugin/marketplace.json must list envelope with source "./"')

# Codex: its own format, token forwarded from the user's environment.
codex = load_json(".codex-plugin/plugin.json")
check_version(".codex-plugin/plugin.json", codex)
if isinstance(codex, dict):
    if codex.get("name") != "envelope":
        err('.codex-plugin/plugin.json name must be "envelope"')
    if codex.get("skills") != "./skills/":
        err('.codex-plugin/plugin.json skills must be "./skills/"')
    if codex.get("mcpServers") != "./.codex-plugin/mcp.json":
        err('.codex-plugin/plugin.json mcpServers must be "./.codex-plugin/mcp.json"')
if (root / "plugin.json").exists():
    err("root plugin.json would switch Codex to the portable format, which cannot forward the token")

codex_mcp = load_json(".codex-plugin/mcp.json")
if isinstance(codex_mcp, dict):
    server = ((codex_mcp.get("mcpServers") or {}).get("envelope")) or {}
    if server.get("command") != "envelope" or server.get("args") != ["mcp"]:
        err(".codex-plugin/mcp.json must run `envelope mcp`")
    if server.get("env_vars") != ["ENVELOPE_AGENT_TOKEN"]:
        err('.codex-plugin/mcp.json env_vars must be ["ENVELOPE_AGENT_TOKEN"]')
    if server.get("env"):
        err(".codex-plugin/mcp.json must not set env (Codex passes env values literally)")

codex_mkt = load_json(".agents/plugins/marketplace.json")
if isinstance(codex_mkt, dict):
    listed = codex_mkt.get("plugins")
    entries = [p for p in listed if isinstance(p, dict)] if isinstance(listed, list) else []
    if codex_mkt.get("name") != "envelope":
        err('.agents/plugins/marketplace.json name must be "envelope"')
    if not any(
        p.get("name") == "envelope"
        and isinstance(p.get("source"), dict)
        and p["source"].get("path") == "./"
        for p in entries
    ):
        err('.agents/plugins/marketplace.json must list envelope with source path "./"')

for rel in (".claude-plugin/plugin.json", ".codex-plugin/mcp.json", "mcp.json"):
    path = root / rel
    if path.is_file() and re.search(r"envtok_[A-Za-z0-9]", path.read_text()):
        err(f"{rel} must not contain an agent token")

if errors:
    print("Agent plugin packaging checklist failed:", file=sys.stderr)
    print("\n".join(f"- {e}" for e in errors), file=sys.stderr)
    sys.exit(1)

print("OK: agent plugin packaging checklist passed (Claude Code, Codex, Cursor).")
PY
