#!/usr/bin/env python3
"""Mock Responses API server for exercising codex-gui without a real model.

Usage:
    python3 mock_responses.py --port 18080

Then point a throwaway CODEX_HOME at it (see `write_config` below or run
with `--write-config DIR`). The reply depends on the last user message:

    "markdown"  rich markdown (headings, lists, code, table, quote)
    "run ..."   asks to run a shell command (exercises exec + approvals)
    "exec ..."  runs a shell command inside the sandbox (no approval)
    "bg ..."    like "exec", but yields after half a second so a long
                command keeps running as a background terminal
                (e.g. "bg sleep 30"; needs the unified exec tool)
    "patch"     proposes an apply_patch edit (exercises file approvals)
    "plan"      updates the plan ("plan slow": the follow-up streams slowly)
    "ask"       calls request_user_input (if offered)
    "tab"       calls the codex_gui cross-tab tools (if offered)
    "tabsend X" lists open tabs, then sends X to the first other tab and
                waits for its reply ("tabpost X" does not wait;
                "tabsend:5 X" waits at most 5 seconds)
    "tabread"   reads this thread's cross-tab mailbox
    "spawn ..." spawns a sub-agent whose first message is the rest
                (e.g. "spawn run ls" makes the sub-agent ask for approval)
    "agent"     spawns a sub-agent (multi_agent_v1.spawn_agent, if offered)
    "slow"      streams slowly for ~6 seconds (exercises interrupt/steer)
    anything    a short streamed reply that echoes the message

A message from another tab (a <codex_gui_message> block) is answered as if
its inner message had been typed, so "tabsend slow" makes the target slow.

After a tool call, the follow-up request (which carries the tool output)
gets a short summary reply.

A request with a JSON schema output format (`text.format.type ==
"json_schema"`, e.g. the GUI's /recap) is answered with a JSON object that
matches the schema.
"""

import argparse
import html
import json
import os
import re
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

COUNTER = {"n": 0}
LOCK = threading.Lock()


def next_id(prefix):
    with LOCK:
        COUNTER["n"] += 1
        return f"{prefix}_{COUNTER['n']}"


def sse(event):
    return f"event: {event['type']}\ndata: {json.dumps(event)}\n\n".encode()


def created(rid):
    return {"type": "response.created", "response": {"id": rid}}


def completed(rid, tokens=1200):
    return {
        "type": "response.completed",
        "response": {
            "id": rid,
            "usage": {
                "input_tokens": tokens,
                "input_tokens_details": {"cached_tokens": tokens // 3},
                "output_tokens": 180,
                "output_tokens_details": {"reasoning_tokens": 40},
                "total_tokens": tokens + 180,
            },
        },
    }


def message_events(text, chunk=12, delay=0.0):
    mid = next_id("msg")
    yield {
        "type": "response.output_item.added",
        "item": {"type": "message", "role": "assistant", "id": mid, "content": [{"type": "output_text", "text": ""}]},
    }
    for i in range(0, len(text), chunk):
        if delay:
            time.sleep(delay)
        yield {"type": "response.output_text.delta", "delta": text[i : i + chunk], "item_id": mid}
    yield {
        "type": "response.output_item.done",
        "item": {"type": "message", "role": "assistant", "id": mid, "content": [{"type": "output_text", "text": text}]},
    }


def reasoning_events(summary):
    rid = next_id("rs")
    yield {
        "type": "response.output_item.added",
        "item": {"type": "reasoning", "id": rid, "summary": []},
    }
    yield {"type": "response.reasoning_summary_part.added", "item_id": rid, "summary_index": 0}
    yield {"type": "response.reasoning_summary_text.delta", "item_id": rid, "delta": summary, "summary_index": 0}
    yield {
        "type": "response.output_item.done",
        "item": {"type": "reasoning", "id": rid, "summary": [{"type": "summary_text", "text": summary}], "encrypted_content": None},
    }


def function_call(name, arguments, namespace=None):
    item = {"type": "function_call", "call_id": next_id("call"), "name": name, "arguments": json.dumps(arguments)}
    if namespace:
        item["namespace"] = namespace
    return {"type": "response.output_item.done", "item": item}


def custom_tool_call(name, text):
    return {
        "type": "response.output_item.done",
        "item": {"type": "custom_tool_call", "call_id": next_id("call"), "name": name, "input": text},
    }


MARKDOWN = """# Summary

Here is a **rich** reply with `inline code`, *emphasis*, and a [link](https://example.com).

## Steps

1. Read `src/main.rs`
2. Update the parser
   - handle `Vec<String>` inputs
   - keep ~~old~~ behaviour behind a flag
3. Run the tests

```rust
fn main() {
    println!("hello from a code block");
    let config = load_config(&std::env::args().collect::<Vec<_>>(), "codex-gui", /*strict*/ true, /*verbose*/ false, Duration::from_secs(30));
}
```

> Note: block quotes render as their own block.

| File | Change |
|------|--------|
| src/lib.rs | +12 -3 |
| README.md | +1 |

---

Done. See [docs](https://developers.openai.com/codex) for more.
"""


def tool_names(body):
    names = set()
    for tool in body.get("tools") or []:
        if tool.get("type") == "namespace":
            for inner in tool.get("tools") or []:
                names.add(f"{tool.get('name')}.{inner.get('name')}")
        else:
            names.add(tool.get("name") or tool.get("type"))
    return names


def last_user_text(body):
    for item in reversed(body.get("input") or []):
        if item.get("type") == "message" and item.get("role") == "user":
            parts = item.get("content") or []
            texts = [p.get("text", "") for p in parts if p.get("type") == "input_text"]
            text = "\n".join(t for t in texts if t)
            if text:
                return text
        if item.get("type") in ("function_call_output", "custom_tool_call_output"):
            return None
    return ""


def unwrap_gui_message(text):
    """Inner message of a codex-gui cross-tab block, else the text itself."""
    match = re.search(r"<codex_gui_message>.*?<message>(.*?)</message>", text, re.S)
    return html.unescape(match.group(1)) if match else text


def plan_events(body, text):
    rid = next_id("resp")
    yield created(rid)
    lowered = unwrap_gui_message(text).lower()
    names = tool_names(body)
    if lowered.startswith("markdown"):
        yield from reasoning_events("**Planning the reply**\n\nI will show markdown features.")
        yield from message_events(MARKDOWN)
    elif lowered.startswith("run"):
        command = text[3:].strip() or "ls -la"
        if "exec_command" in names:
            yield function_call("exec_command", {"cmd": command, "sandbox_permissions": "require_escalated", "justification": "Mock asks for approval"})
        else:
            yield function_call("shell", {"command": ["bash", "-lc", command]})
    elif lowered.startswith("exec"):
        command = text[4:].strip() or "ls -la"
        if "exec_command" in names:
            yield function_call("exec_command", {"cmd": command})
        else:
            yield function_call("shell", {"command": ["bash", "-lc", command]})
    elif lowered.startswith("bg"):
        command = text[2:].strip() or "sleep 30"
        if "exec_command" in names:
            yield function_call("exec_command", {"cmd": command, "yield_time_ms": 500})
        else:
            yield from message_events("No unified exec tool was offered; enable features.unified_exec.")
    elif lowered.startswith("patch"):
        patch = (
            "*** Begin Patch\n*** Add File: hello_from_mock.txt\n+Hello from the mock model.\n+Second line.\n*** End Patch\n"
        )
        if "apply_patch" in names:
            yield custom_tool_call("apply_patch", patch)
        elif "exec_command" in names:
            # Models without the apply_patch tool use the shell; core
            # intercepts the heredoc and runs it as a file change.
            yield function_call("exec_command", {"cmd": f"apply_patch <<'EOF'\n{patch}EOF\n"})
        else:
            yield function_call("shell", {"command": ["apply_patch", patch]})
    elif lowered.startswith("plan"):
        yield function_call(
            "update_plan",
            {
                "explanation": "Mock plan",
                "plan": [
                    {"step": "Read the code", "status": "completed"},
                    {"step": "Write the fix", "status": "in_progress"},
                    {"step": "Run tests", "status": "pending"},
                ],
            },
        )
    elif lowered.startswith("ask") and "request_user_input" in names:
        yield function_call(
            "request_user_input",
            {
                "questions": [
                    {
                        "id": "color",
                        "header": "Color",
                        "question": "Which color should the button be?",
                        "options": [
                            {"label": "Blue (Recommended)", "description": "Matches the theme"},
                            {"label": "Green", "description": "Stands out"},
                        ],
                    }
                ]
            },
        )
    elif lowered.startswith("tabsend") or lowered.startswith("tabpost"):
        if "codex_gui.send_message_to_thread" in names:
            yield function_call("list_open_threads", {}, namespace="codex_gui")
        else:
            yield from message_events("No cross-tab tools were offered.")
    elif lowered.startswith("tabread"):
        if "codex_gui.read_thread_mailbox" in names:
            yield function_call("read_thread_mailbox", {"limit": 5}, namespace="codex_gui")
        else:
            yield from message_events("No cross-tab tools were offered.")
    elif lowered.startswith("tab"):
        listed = [n for n in names if n.startswith("codex_gui.")]
        if listed:
            yield function_call("list_open_threads", {}, namespace="codex_gui")
        else:
            yield from message_events("No cross-tab tools were offered.")
    elif lowered.startswith("spawn"):
        # The sub-agent's first message is the rest of the prompt, so
        # "spawn run ls" makes a sub-agent that asks for a command approval.
        prompt = text[5:].strip() or "hello from the parent"
        if "multi_agent_v1.spawn_agent" in names:
            yield function_call("spawn_agent", {"message": prompt}, namespace="multi_agent_v1")
        elif "spawn_agent" in names:
            yield function_call("spawn_agent", {"message": prompt})
        else:
            yield from message_events("No sub-agent tools were offered.")
    elif lowered.startswith("agent") and "multi_agent_v1.spawn_agent" in names:
        yield function_call(
            "spawn_agent",
            {"message": "Check the test suite and report failures."},
            namespace="multi_agent_v1",
        )
    elif lowered.startswith("slow"):
        yield from message_events("Streaming slowly so you can interrupt or steer me. " * 6, chunk=6, delay=0.12)
    else:
        yield from message_events(f"You said: {text}\n\nThis reply comes from the **mock** server.")
    yield completed(rid)


SCHEMA_SAMPLES = {
    "summary": "You tried a few mock prompts and each one got a short reply from the mock model. Nothing is broken or pending.",
    "next_action": "Send another message to keep going.",
}


def schema_sample(schema, name="value"):
    """A value that satisfies a (simple) JSON schema."""
    kind = schema.get("type")
    if isinstance(kind, list):
        kind = next((k for k in kind if k != "null"), "null")
    if kind == "object":
        properties = schema.get("properties") or {}
        return {key: schema_sample(value, key) for key, value in properties.items()}
    if kind == "array":
        return [schema_sample(schema.get("items") or {}, name)]
    if kind == "string":
        if schema.get("enum"):
            return schema["enum"][0]
        text = SCHEMA_SAMPLES.get(name, f"Mock {name}")
        limit = schema.get("maxLength")
        return text[:limit] if limit else text
    if kind in ("integer", "number"):
        return schema.get("minimum", 1)
    if kind == "boolean":
        return True
    return None


def structured_events(body):
    """Answers a request whose output must match `text.format.schema`."""
    rid = next_id("resp")
    yield created(rid)
    schema = ((body.get("text") or {}).get("format") or {}).get("schema") or {}
    if "matches" in (schema.get("properties") or {}):
        text = last_user_text(body) or ""
        query = json.loads(text.split("Query: ", 1)[1].splitlines()[0]).lower()
        matches = []
        for line in text.splitlines():
            try:
                row = json.loads(line)
            except (ValueError, TypeError):
                continue
            if isinstance(row, dict) and "id" in row and query in (row.get("title", "") + " " + row.get("history", "")).lower():
                if row["id"] not in matches:
                    matches.append(row["id"])
        yield from message_events(json.dumps({"matches": matches}))
    else:
        yield from message_events(json.dumps(schema_sample(schema)))
    yield completed(rid)


def wants_json_schema(body):
    return ((body.get("text") or {}).get("format") or {}).get("type") == "json_schema"


def turn_user_text(body):
    """The user message that started the current turn (ignores tool items)."""
    for item in reversed(body.get("input") or []):
        if item.get("type") == "message" and item.get("role") == "user":
            parts = item.get("content") or []
            return "\n".join(p.get("text", "") for p in parts if p.get("type") == "input_text")
    return ""


def cross_tab_send(text, output):
    """After list_open_threads in a "tabsend"/"tabpost" turn: message the first other tab."""
    try:
        threads = json.loads(output).get("threads") or []
    except (TypeError, ValueError):
        threads = []
    # The first other tab even when it refuses messages, so the GUI's
    # refusal is exercised too.
    target = next((t for t in threads if not t.get("is_self")), None)
    if target is None:
        return None
    match = re.match(r"tab(send|post)(?::(\d+))?\s*(.*)", text, re.I | re.S)
    mode, timeout, message = match.groups() if match else ("send", None, "")
    return function_call(
        "send_message_to_thread",
        {
            "target": target["thread_id"],
            "message": message.strip() or "Hello from the mock agent in another tab.",
            "wait_for_reply": mode.lower() == "send",
            "timeout_seconds": int(timeout or 120),
        },
        namespace="codex_gui",
    )


def followup_events(body):
    rid = next_id("resp")
    yield created(rid)
    items = body.get("input") or []
    outputs = [i for i in items if i.get("type") in ("function_call_output", "custom_tool_call_output")]
    last = outputs[-1] if outputs else {}
    output = last.get("output")
    if isinstance(output, list):
        output = " ".join(str(p.get("text", "")) for p in output if isinstance(p, dict))
    call = next((i for i in items if i.get("type") == "function_call" and i.get("call_id") == last.get("call_id")), {})
    # A message from another tab is handled as its inner text, so chains of
    # "tabpost tabpost ..." keep forwarding.
    text = unwrap_gui_message(turn_user_text(body))
    if text.lower().startswith("plan slow"):
        yield from message_events("Working through the plan step by step. " * 8, chunk=6, delay=0.12)
        yield completed(rid)
        return
    if call.get("name") == "list_open_threads" and text.lower().startswith(("tabsend", "tabpost")):
        send = cross_tab_send(text, output)
        if send is not None:
            yield send
            yield completed(rid)
            return
    snippet = (str(output) or "")[:400]
    yield from message_events(f"The tool returned:\n\n```\n{snippet}\n```\n\nAll done.")
    yield completed(rid)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):  # noqa: N802
        sys.stderr.write("mock: " + (fmt % args) + "\n")

    def do_GET(self):  # noqa: N802
        if self.path.rstrip("/").endswith("/models"):
            body = json.dumps({"object": "list", "data": [{"id": "mock-model", "object": "model"}]}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        self.send_response(404)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_POST(self):  # noqa: N802
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b"{}"
        try:
            body = json.loads(raw or b"{}")
        except json.JSONDecodeError:
            body = {}
        if not self.path.rstrip("/").endswith("/responses"):
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        text = last_user_text(body)
        if wants_json_schema(body):
            events = structured_events(body)
        elif text is None:
            events = followup_events(body)
        else:
            events = plan_events(body, text)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        try:
            for event in events:
                self.wfile.write(sse(event))
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass
        self.close_connection = True


def write_config(codex_home, port):
    os.makedirs(codex_home, exist_ok=True)
    with open(os.path.join(codex_home, "config.toml"), "w") as f:
        f.write(
            f"""model = "mock-model"
model_provider = "mock"
approval_policy = "on-request"
sandbox_mode = "workspace-write"

[model_providers.mock]
name = "Mock"
base_url = "http://127.0.0.1:{port}/v1"
wire_api = "responses"
requires_openai_auth = false
request_max_retries = 0
stream_max_retries = 0

[tools.update_plan]
enabled = true
"""
        )


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=18080)
    parser.add_argument("--write-config", metavar="CODEX_HOME")
    args = parser.parse_args()
    if args.write_config:
        write_config(args.write_config, args.port)
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    sys.stderr.write(f"mock responses server on http://127.0.0.1:{args.port}/v1\n")
    server.serve_forever()


if __name__ == "__main__":
    main()
