#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Generate mira-docs/settings-reference.md from config/mira_config.schema.json.

The schema is the single source of truth for global/operator settings. This
projects it into the bundled Markdown reference so the two can never drift:
run it whenever the schema changes (a test asserts the file is up to date).

The schema descriptions may carry dev-facing internal jargon (design-phase
codenames, §-references); `polish()` strips that and auto-backticks config
paths / --flags / 'enum' literals so the projected doc stays user-facing.

Usage:
    python3 scripts/gen_settings_reference.py           # write the file
    python3 scripts/gen_settings_reference.py --check    # exit 1 if stale
"""
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCHEMA = ROOT / "config" / "mira_config.schema.json"
OUT = ROOT / "mira-docs" / "settings-reference.md"

HEADER = """# MIRA settings reference

> Auto-generated from `config/mira_config.schema.json` — the single source of truth for global/operator settings. Do not hand-edit; regenerate with `scripts/gen_settings_reference.py` when the schema changes.

> These are **server-wide (operator) settings** stored in `mira_config.json`. Viewing/changing them requires admin access. Secret values (API keys, tokens, passwords) are always redacted on read.
"""


def polish(text: str, top_keys: frozenset = frozenset()) -> str:
    """Strip internal jargon and auto-backtick config paths / flags / enums.

    `top_keys` is the set of top-level schema keys; a dotted token is only
    backticked as a config path when its first segment is one of them, so
    example hostnames (`dc.example.com`) aren't mistaken for settings.
    """
    if not text:
        return text
    t = text.strip()
    # Leading design-phase prefix ("Phase-1: ") → strip and re-capitalize.
    m = re.match(r"^Phase[-\s]?\d+\s*[:\-—]?\s+(.*)$", t, re.S)
    if m:
        r = m.group(1)
        t = (r[:1].upper() + r[1:]) if r else r
    # Inline internal jargon (safe, unambiguous codenames).
    t = re.sub(r"\bPhase[-\s]?\d+\s+(?=[a-z(])", "", t)            # inline "Phase 1 the"
    t = re.sub(r"\s*\(§[\d.]+\)", "", t)                      # (§4.5)
    t = re.sub(r"\s*\(P\d+[a-z]?\)", "", t)                        # (P2b)
    t = re.sub(r"\s{2,}", " ", t).strip()
    # Auto-backtick (skip anything already inside backticks by requiring a
    # non-backtick, non-word boundary on the left).
    t = re.sub(r"(?<![`\w.])(--[a-z][a-z0-9-]*(?:\s+--[a-z][a-z0-9-]*)*)", r"`\1`", t)  # --flags (runs)

    def _path(m):
        tok = m.group(1)
        return f"`{tok}`" if tok.split(".")[0] in top_keys else tok

    t = re.sub(r"(?<![`\w.])([a-z][a-z0-9_]*(?:\.[a-z0-9_]+)+)(?![`\w])", _path, t)  # config paths
    t = t.replace("`e.g`", "e.g").replace("`i.e`", "i.e")         # abbrevs, not paths
    return t


def type_annot(node: dict) -> str:
    t = node.get("type", "object")
    if isinstance(t, list):
        t = next((x for x in t if x != "null"), t[0])
    enum = node.get("enum")
    if enum:
        vals = ", ".join(f"`{v}`" for v in enum)
        return f"{t}; one of: {vals}"
    return t


def bullet(path: str, node: dict, top: frozenset) -> str:
    return f"- **`{path}`** ({type_annot(node)}) — {polish(node.get('description', ''), top)}"


def emit_children(props: dict, prefix: str, out: list, top: frozenset):
    """Every descendant (nested object or leaf) gets a bullet."""
    for k, v in props.items():
        if not isinstance(v, dict):
            continue
        path = f"{prefix}{k}"
        out.append(bullet(path, v, top))
        if v.get("type") == "object" and "properties" in v:
            emit_children(v["properties"], path + ".", out, top)


def generate() -> str:
    schema = json.loads(SCHEMA.read_text())
    props = schema.get("properties", {})
    top = frozenset(props.keys())
    lines = [HEADER]
    for key, node in props.items():
        if not isinstance(node, dict):
            continue
        lines.append(f"\n## {key}\n")
        is_obj = node.get("type") == "object" and "properties" in node
        if is_obj:
            desc = polish(node.get("description", ""), top)
            if desc:
                lines.append(f"_{desc}_\n")
            body: list = []
            emit_children(node["properties"], key + ".", body, top)
            lines.append("\n".join(body))
        else:
            # Top-level scalar/array: a single bullet carries its description.
            lines.append(bullet(key, node, top))
    return "\n".join(lines).rstrip() + "\n"


def main():
    content = generate()
    if "--check" in sys.argv:
        current = OUT.read_text() if OUT.exists() else ""
        if current != content:
            print("settings-reference.md is STALE — run scripts/gen_settings_reference.py", file=sys.stderr)
            sys.exit(1)
        print("settings-reference.md is up to date.")
        return
    OUT.write_text(content)
    print(f"wrote {OUT.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
