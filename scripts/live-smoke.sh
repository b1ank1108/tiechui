#!/bin/sh
# Manual only. This script intentionally uses the configured provider's credentials.
set -eu
if [ "${1:-}" = "--help" ] || [ -z "${TIECHUI_SMOKE_MODEL:-}" ]; then
    echo 'Usage: TIECHUI_SMOKE_MODEL=<provider>/<id> [TIECHUI_BIN=<tc-path>] scripts/live-smoke.sh [--mcp <server>]'
    echo 'Runs two bash turns, checks the model-visible skills catalog, resumes, and prints usage including cache reads.'
    echo 'Optional --mcp also requires a successful tool call from that configured server.'
    echo 'Requires Python 3.9+, a configured model and visible skill, and provider credential environment variables; tc resolves config.'
    [ "${1:-}" = "--help" ] && exit 0
    exit 2
fi
if [ -n "${CI:-}" ]; then
    echo 'live-smoke is manual only; refusing to run in CI.' >&2
    exit 2
fi
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
exec python3 - "$repo" "$@" <<'PY'
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

repo = Path(sys.argv[1])
parser = argparse.ArgumentParser()
parser.add_argument("--mcp", metavar="SERVER")
args = parser.parse_args(sys.argv[2:])
model = os.environ["TIECHUI_SMOKE_MODEL"]
binary = str(Path(os.environ.get("TIECHUI_BIN", str(repo / "target/release/tc"))).resolve())

def inspect(cwd, command):
    result = subprocess.run([binary, *command, "--cwd", cwd, "--json"],
                            text=True, stdout=subprocess.PIPE, check=True)
    return json.loads(result.stdout)

def turn(cwd, prompt, extra, number, skill=None, mcp_tools=()):
    result = subprocess.run([binary, "exec", prompt, "--model", model, "--cwd", cwd,
                             "--json", *extra], text=True, stdout=subprocess.PIPE)
    if result.returncode:
        sys.exit(result.returncode)
    events = [json.loads(line) for line in result.stdout.splitlines()]
    calls = []
    successes = set()
    messages = []
    for envelope in events:
        event = envelope["event"]
        if event["kind"] == "item_completed":
            item = event["item"]
            if item["kind"] == "assistant_message":
                print(item["text"])
                messages.append(item["text"])
            if item["kind"] == "tool_call":
                calls.append(item)
            if item["kind"] == "tool_result" and not item["is_error"]:
                successes.add(item["call_id"])
        if event["kind"] == "token_usage_updated":
            print(f"Turn {number} usage: {json.dumps(event['last'], sort_keys=True)}", flush=True)
    if not any(call["name"] == "bash" and call["id"] in successes for call in calls):
        sys.exit(f"Turn {number} did not successfully call bash")
    if skill is not None:
        # The description and location are deliberately absent from the user prompt.
        # Reject other tools/commands so the answer must come from the catalog.
        if any(call["name"] != "bash" or call["arguments"].get("command") != "printf 'smoke-ok'"
               for call in calls):
            sys.exit("Skills check used a tool or command beyond the requested printf")
        text = "\n".join(messages).strip()
        if text.startswith("```json\n") and text.endswith("\n```"):
            text = text[len("```json\n"):-len("\n```")]
        try:
            reported = json.loads(text)
        except ValueError:
            sys.exit("Skills check did not return the requested JSON")
        expected = {"name": skill["name"], "description": skill["description"],
                    "location": skill["path"]}
        if reported != expected:
            sys.exit("Skills catalog description/location did not reach the model's answer")
        print(f"Skills catalog check passed: {skill['name']}", flush=True)
    if mcp_tools and not any(call["name"] in mcp_tools and call["id"] in successes for call in calls):
        sys.exit(f"Turn {number} did not successfully call a tool from {args.mcp}")
    return events[0]["thread_id"]

local = repo / ".local"
local.mkdir(exist_ok=True)
with tempfile.TemporaryDirectory(prefix="tc-live-smoke-", dir=local) as cwd:
    catalog = inspect(cwd, ["skills", "list"])
    skill = next((entry for entry in catalog["skills"]
                  if not entry["disable_model_invocation"]), None)
    if skill is None:
        sys.exit("Skills check requires a model-visible skill, for example under ~/.agents/skills")
    mcp_tools = []
    if args.mcp:
        servers = inspect(cwd, ["mcp", "list"])["servers"]
        server = next((entry for entry in servers if entry["name"] == args.mcp), None)
        if server is None or server["state"] != "ready" or not server["tools"]:
            sys.exit(f"MCP server {args.mcp!r} is missing, not ready, or has no tools")
        mcp_tools = server["tools"]
    session = turn(cwd,
                   "Use bash to run exactly printf 'smoke-ok'. Then, from your available_skills "
                   f"catalog, report skill {json.dumps(skill['name'])}. "
                   "Do not read files or run any other command. Reply only with a JSON object "
                   "with keys name, description, location from that catalog entry. "
                   "Preserve exact whitespace in the values and decode XML entities into JSON strings.",
                   [], 1, skill=skill)
    prompt = "Use bash to run printf 'smoke-two'. What did the previous command print? Reply briefly."
    if mcp_tools:
        prompt += (f" Also make one harmless read-only tool call from MCP server {json.dumps(args.mcp)}. "
                   f"Its exposed tool names are {json.dumps(mcp_tools)}. "
                   "Servers start asynchronously in this new process: if its tools are not yet in your "
                   "tool catalog, use bash to sleep 1 second and check the next request's catalog, "
                   "up to ten times. Use a tool only once it is available, and report its result.")
    turn(cwd, prompt, ["--resume", session], 2, mcp_tools=mcp_tools)
PY
