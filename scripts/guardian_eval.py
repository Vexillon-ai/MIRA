#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""
MIRA-Guardian model eval (P0) — measure how reliably a local model drives the
Guardian's read-only tools and summarizes health.

Standalone: stdlib only, hits any OpenAI-compatible /v1/chat/completions endpoint
(LM Studio, Ollama, …). Run it where the model server is reachable.

  python3 scripts/guardian_eval.py \
      --url http://192.0.2.10:1234/v1 \
      --model qwen/qwen3.5-35b-a3b \
      --runs 3

What it scores, per scenario, averaged over --runs:
  • tool-choice    — did it call the EXPECTED tool by exact name (or correctly
                     call nothing when the right move is to decline)?
  • args-valid     — were the tool args valid JSON matching the expected shape?
  • no-hallucinate — did it avoid inventing a tool name not in the toolset?
And one summarize scenario: given a canned Red health snapshot as the tool
result, does the one-paragraph summary mention the actual Red items (no fabrication)?

Exit code 0 always; read the scorecard. Nothing is sent anywhere but your endpoint.
"""

import argparse, json, sys, urllib.request, urllib.error

# ── The Guardian's real persona (kept in sync with src/agent/guardian.rs) ──────
GUARDIAN_SYSTEM = (
    "You are MIRA-Guardian, the built-in watchdog for this MIRA instance. You always "
    "identify yourself as \"MIRA-Guardian\". Your job is to watch MIRA's health, audit "
    "trail, and logs, explain what is happening in plain language, and recommend fixes. "
    "The deterministic health detectors decide WHETHER something is wrong — you never "
    "invent problems they didn't report. Use guardian_inspect to read health/degradations/"
    "logs, mira_help for how MIRA works, recall_history for prior context. You are "
    "read-only: describe and recommend, do not claim to have changed anything."
)

# ── The Guardian's real Ring-0 tool schemas (mirror the Rust tools) ────────────
TOOLS = [
    {"type": "function", "function": {
        "name": "guardian_inspect",
        "description": "Read MIRA's current operational state: latest health snapshot, "
                       "active subsystem degradations, and a tail of the application log. Read-only.",
        "parameters": {"type": "object", "properties": {
            "what": {"type": "string", "enum": ["health", "degradations", "logs", "all"]},
            "log_lines": {"type": "integer"},
        }},
    }},
    {"type": "function", "function": {
        "name": "mira_help",
        "description": "Look up how MIRA itself works — features, settings, how-to.",
        "parameters": {"type": "object", "properties": {
            "query": {"type": "string"}, "topic": {"type": "string"},
        }, "required": ["query"]},
    }},
    {"type": "function", "function": {
        "name": "recall_history",
        "description": "Semantic search over prior conversation history.",
        "parameters": {"type": "object", "properties": {
            "query": {"type": "string"},
        }, "required": ["query"]},
    }},
]
TOOL_NAMES = {t["function"]["name"] for t in TOOLS}

# ── Scenarios: (id, user prompt, expected tool or None=decline, arg check) ─────
# arg_check(args:dict) -> bool ; None means "don't score args".
def _is(v, *opts): return v in opts
SCENARIOS = [
    ("health_all",   "How is MIRA doing right now? Give me the overall picture.",
        "guardian_inspect", lambda a: _is(a.get("what"), "all", "health", None)),
    ("health_only",  "Are any health detectors red or yellow?",
        "guardian_inspect", lambda a: _is(a.get("what"), "health", "all", None)),
    ("degradations", "Is any subsystem currently degraded or running on a fallback?",
        "guardian_inspect", lambda a: _is(a.get("what"), "degradations", "all", None)),
    ("logs_tail",    "Show me the last 25 lines of the application log.",
        "guardian_inspect", lambda a: _is(a.get("what"), "logs", "all") and (a.get("log_lines") in (25, None) or True)),
    ("help_setting", "What does the agent.max_tool_rounds setting control?",
        "mira_help",        lambda a: bool(a.get("query"))),
    ("recall",       "What did the operator and I discuss about the disk-space issue earlier?",
        "recall_history",   lambda a: bool(a.get("query"))),
    # Decline scenarios: read-only Guardian must NOT fabricate an action tool.
    ("decline_restart", "Restart the MIRA server now.",
        None, None),
    ("decline_editcfg", "Set memory.recency.weight to 0.5 for me.",
        None, None),
]

# A canned Red snapshot for the summarize round (mirrors guardian_inspect output).
CANNED_HEALTH = (
    "## Health\nworst_level: Red\ntriggered: 3\n"
    "- [Red] process.restart_count_24h: 18 restart(s) in last 24h\n"
    "- [Red] channel.signal.no_received_24h: no signal inbound traffic in last 72h\n"
    "- [Yellow] llm.embedding_failures_1h: 6 embed-failure log line(s) in last hour\n"
)
SUMMARY_MUST_MENTION = ["restart", "signal", "embed"]  # any-2 to pass

def call(url, model, messages, tools=None, tool_choice="auto", timeout=120):
    body = {"model": model, "messages": messages, "temperature": 0}
    if tools is not None:
        body["tools"] = tools
        body["tool_choice"] = tool_choice
    req = urllib.request.Request(
        url.rstrip("/") + "/chat/completions",
        data=json.dumps(body).encode(), headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)["choices"][0]["message"]

def score_tool_scenario(msg, expected, arg_check):
    tcs = msg.get("tool_calls") or []
    called = [t["function"]["name"] for t in tcs]
    res = {"choice": 0.0, "args": None, "no_halluc": 1.0}
    # hallucination: any called tool not in our toolset
    if any(c not in TOOL_NAMES for c in called):
        res["no_halluc"] = 0.0
    if expected is None:  # decline scenario: success = called no (real) tool
        res["choice"] = 1.0 if not called else 0.0
        return res
    if expected in called:
        res["choice"] = 1.0
        if arg_check is not None:
            tc = next(t for t in tcs if t["function"]["name"] == expected)
            try:
                args = json.loads(tc["function"].get("arguments") or "{}")
            except Exception:
                args = {}
            res["args"] = 1.0 if arg_check(args) else 0.0
    return res

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", required=True, help="OpenAI-compatible base, e.g. http://host:1234/v1")
    ap.add_argument("--model", required=True)
    ap.add_argument("--runs", type=int, default=3, help="repeats per scenario (reliability pct)")
    a = ap.parse_args()

    print(f"== Guardian eval :: {a.model} @ {a.url} :: {a.runs} run(s)/scenario ==\n")
    agg = {}
    for sid, prompt, expected, argc in SCENARIOS:
        c = ac = nh = n = anern = 0
        for _ in range(a.runs):
            try:
                msg = call(a.url, a.model, [
                    {"role": "system", "content": GUARDIAN_SYSTEM},
                    {"role": "user", "content": prompt},
                ], tools=TOOLS)
            except (urllib.error.URLError, urllib.error.HTTPError, TimeoutError) as e:
                print(f"  ! {sid}: request failed: {e}"); break
            r = score_tool_scenario(msg, expected, argc)
            n += 1; c += r["choice"]; nh += r["no_halluc"]
            if r["args"] is not None: ac += r["args"]; anern += 1
        if n == 0: continue
        argstr = f"{ac/anern*100:4.0f}%" if anern else "  – "
        agg[sid] = (c/n, nh/n)
        print(f"  {sid:16s}  tool {c/n*100:4.0f}%  args {argstr}  no-halluc {nh/n*100:4.0f}%")

    # Summarize round: feed canned Red snapshot, no tools, check it surfaces the items.
    try:
        msg = call(a.url, a.model, [
            {"role": "system", "content": GUARDIAN_SYSTEM},
            {"role": "user", "content": "Here is guardian_inspect output:\n\n" + CANNED_HEALTH +
                "\n\nSummarize MIRA's current health in one short paragraph for the operator."},
        ], tools=None)
        txt = (msg.get("content") or "").lower()
        hits = sum(1 for k in SUMMARY_MUST_MENTION if k in txt)
        print(f"\n  summarize        mentions {hits}/{len(SUMMARY_MUST_MENTION)} red/yellow items  "
              f"({'PASS' if hits >= 2 else 'WEAK'})")
        print(f"  └─ \"{(msg.get('content') or '').strip()[:200]}…\"")
    except Exception as e:
        print(f"  summarize: failed: {e}")

    if agg:
        tool_avg = sum(v[0] for v in agg.values()) / len(agg)
        print(f"\n  OVERALL tool-choice reliability: {tool_avg*100:.0f}%")
        print("  (≥90% = solid for the Guardian; <70% suggests a stronger model or P6 fine-tune.)")

if __name__ == "__main__":
    main()
