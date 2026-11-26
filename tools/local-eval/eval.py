#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Score local chat completions with deterministic, in-memory workspace fixtures."""

import argparse
from collections import defaultdict
from concurrent.futures import ThreadPoolExecutor
import hashlib
import http.client
import json
from pathlib import Path, PurePosixPath
from threading import Lock
import re
import socket
import time

HERE = Path(__file__).resolve().parent
PROFILES = json.loads((HERE.parent.parent / "src" / "local_profiles.json").read_text())
MODELS = ("minicpm5-1b", "qwen3.5-0.8b", "lfm3-vl-2b")
BASE = (
    "You are the enzimOS assistant, working in the folder the user chose (the workspace). "
    'Use the tools to look at and change it. Paths are relative to the workspace, such as '
    '"todo.txt" or "notes/plan.md"; call files_list with a null path to see what is there. '
    "When a tool reports a path error, correct the path and call it again. "
    "Answer briefly, from what the tools returned."
)
PROMPT = (
    "You are the enzimOS assistant. Follow the requested format exactly. "
    "For summaries and rewrites, use only the supplied text. "
    'If information is missing, say so.'
)
WORKSPACE = (
    " Use relative workspace paths. Use tools for file requests. "
    "Report success only after a successful tool result."
)
EXAMPLES = (
    '\nExamples of tool arguments: list the workspace → files_list({}); '
    'read report.md → file_read({"path":"report.md"}); '
    'create hello.txt with Hi → file_write({"path":"hello.txt","text":"Hi","expected_sha256":null}). '
    "To replace a file, read it first and copy its sha256 into expected_sha256."
)
MODEL_PROMPT = {
    "minicpm5-1b": " Answer directly and briefly.",
    "qwen3.5-0.8b": " Return just the requested answer.",
    "lfm3-vl-2b": " Keep the reply concise and factual.",
}
FIXTURE = {
    "agenda.txt": "The meeting is in room B17. Bring a notebook.\n",
    "notes/handover.txt": "Locker number: 46\n",
    "stock.txt": "Spare label color: teal\n",
}


def schema(name, description, properties, required):
    return {"type": "function", "function": {
        "name": name, "description": description,
        "parameters": {"type": "object", "properties": properties,
                       "required": required, "additionalProperties": False},
    }}


def schemas(short):
    path = {"type": "string"}
    optional_path = path if short else {"type": ["string", "null"]}
    digest = {"type": ["string", "null"]}
    tools = [
        schema("files_list",
               "List a folder. Omit path for the workspace root." if short else
               "List one folder of the workspace without following symlinks. `path` is a folder inside the workspace; null lists the workspace itself.",
               {"path": optional_path}, []),
        schema("file_read",
               "Read a text file in the workspace." if short else
               'Read one bounded UTF-8 text file of the workspace. `path` is relative to the workspace: a file name such as "todo.txt", or folder/name for a file in a folder.',
               {"path": path}, ["path"]),
        schema("file_write",
               "Write exact text. Use null for a new file or its read sha256 to replace it." if short else
               "Replace reviewed text and retain the displaced file as a backup; sha256 null creates without overwrite.",
               {"path": path, "text": {"type": "string"}, "expected_sha256": digest},
               ["path", "text", "expected_sha256"]),
    ]
    if not short:
        tools.append(schema(
            "command", "Run argv in a bounded sandbox; no credentials, GUI bus or host home is mounted.",
            {"argv": {"type": "array", "items": {"type": "string"}},
             "writable": {"type": "boolean"}, "network": {"type": "boolean"}}, ["argv"]))
    return tools


class UnixHTTP(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=180)
        self.path = str(path)

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def request(socket_path, body):
    connection = UnixHTTP(socket_path)
    try:
        connection.request("POST", "/v1/chat/completions", json.dumps(body),
                           {"Content-Type": "application/json"})
        response = connection.getresponse()
        value = json.load(response)
        if response.status != 200:
            raise RuntimeError(f"HTTP {response.status}: {value}")
        return value
    finally:
        connection.close()


def sha(text):
    return hashlib.sha256(text.encode()).hexdigest()


def execute(files, name, args):
    path = args.get("path")
    if name == "files_list" and path in ("/", "./", ".", ""):
        path = None
    if path is not None and (not isinstance(path, str) or path.startswith("/") or
                            any(part in ("", ".", "..") for part in path.split("/"))):
        return {"ok": False, "error": "workspace-relative path required"}
    if name == "file_read" and path in files:
        return {"ok": True, "result": {"text": files[path], "sha256": sha(files[path])}}
    if name == "files_list":
        prefix = f"{path}/" if path else ""
        entries = {}
        for filename in files:
            if filename.startswith(prefix):
                first, separator, _ = filename[len(prefix):].partition("/")
                entries[first] = "directory" if separator else "file"
        if entries:
            return {"ok": True, "result": [{"name": key, "kind": entries[key]}
                                            for key in sorted(entries)]}
        if path in files:
            return {"ok": True, "result": {"file": path, "read": execute(files, "file_read", args)["result"]}}
    elif name == "file_write" and path:
        parent = str(PurePosixPath(path).parent)
        expected = sha(files[path]) if path in files else None
        if parent in (".", "notes") and args.get("expected_sha256") == expected:
            files[path] = args["text"]
            return {"ok": True, "result": {"bytes": len(args["text"].encode()),
                                           "sha256": sha(args["text"]), "backup": None}}
        return {"ok": False, "error": "file changed; review again"}
    error = "no such file or folder in the workspace; call files_list with a null path to see what is there"
    return {"ok": False, "error": error}


def valid_args(tool, args):
    types = {"string": str, "null": type(None), "array": list, "boolean": bool}
    rule = tool["parameters"]
    if not isinstance(args, dict) or not set(rule["required"]) <= args.keys():
        return False
    for key, value in args.items():
        if key not in rule["properties"]:
            return False
        kinds = rule["properties"][key]["type"]
        if isinstance(kinds, str):
            kinds = [kinds]
        if type(value) not in [types[kind] for kind in kinds]:
            return False
    return True


def profile(variant, model, case):
    file_task = bool(case.get("calls"))
    tools = schemas(short=variant in ("schema", "decode", "gated", "thinking", "required"))
    prompt = BASE if variant == "baseline" else PROMPT + MODEL_PROMPT[model] + WORKSPACE
    if variant not in ("baseline", "prompt"):
        prompt += EXAMPLES
    if variant in ("gated", "thinking", "required") and not file_task:
        tools = []
        prompt = PROMPT + MODEL_PROMPT[model]
    options = {"chat_template_kwargs": {"enable_thinking": variant == "thinking"}}
    if variant in ("decode", "gated", "thinking", "required"):
        options.update(temperature=0.15, top_p=0.9, repeat_penalty=1.03)
    if variant == "final" and case["category"] == "write" and model == "qwen3.5-0.8b":
        return BASE, schemas(short=False), {"chat_template_kwargs": {"enable_thinking": False}}
    if variant == "final":
        prompt = PROFILES["prompt"] + PROFILES["models"][model]
        tools = []
        if file_task:
            prompt += PROFILES["workspace"] + PROFILES["examples"]
            tools = [{"type": "function", "function": tool} for tool in PROFILES["tools"]]
        options = PROFILES["sampling"]
    return prompt, tools, options


def score(case, text, calls, files):
    normalized = " ".join(text.split())
    exact = case.get("exact")
    if exact:
        pattern = r"(?<!\w)" + re.escape(exact) + r"(?!\w)"
        if case["id"] == "shortcut":
            pattern = r"(?<!\w)Ctrl\s*\+\s*V(?!\w)"
        correctness = bool(re.search(pattern, normalized, re.I))
    else:
        correctness = True
    correctness = (
        correctness
        and all(re.search(pattern, normalized, re.I) for pattern in case.get("require", []))
        and not any(re.search(pattern, normalized, re.I) for pattern in case.get("forbid", []))
    )
    if "file" in case:
        path, expected = case["file"]
        correctness &= files.get(path) == expected
    format_ok = (bool(text) and len(text.split()) <= case.get("max_words", 256)
                 and ("lines" not in case or len(text.splitlines()) == case["lines"])
                 and (not exact or text.casefold() == exact.casefold()))
    expected_calls = list(zip(case.get("calls", []), case.get("paths", [])))
    observed = []
    for call in calls:
        path = (call["args"] or {}).get("path") if call["valid"] else None
        if call["name"] == "files_list" and path in ("/", "./", ".", ""):
            path = None
        observed.append((call["name"], path))
    tool_ok = observed == expected_calls and all(call["valid"] for call in calls)
    tool_ok &= all(call["ok"] for call in calls) or case["id"] == "missing-file"
    return {"correct": correctness, "format": format_ok, "tools": tool_ok,
            "pass": correctness and format_ok and tool_ok}


def run_case(socket_path, model, variant, case, cache=False, max_tokens=256):
    files = FIXTURE.copy()
    prompt, tools, options = profile(variant, model, case)
    messages = [{"role": "system", "content": prompt}, {"role": "user", "content": case["prompt"]}]
    tools_by_name = {tool["function"]["name"]: tool["function"] for tool in tools}
    calls, completions = [], []
    started = time.monotonic()
    text, error = "", None
    try:
        for step in range(5):
            body = dict(model=model, messages=messages, max_tokens=max_tokens, seed=42,
                        cache_prompt=cache, stream=False, **options)
            if tools:
                body["tools"] = tools
                if variant == "required" and step == 0:
                    body["tool_choice"] = "required"
            choice = request(socket_path, body)["choices"][0]
            completions.append(choice)
            message = choice["message"]
            if not message.get("tool_calls"):
                text = (message.get("content") or "").strip()
                break
            messages.append(message)
            for call in message["tool_calls"]:
                function = call["function"]
                name = function["name"]
                try:
                    args = json.loads(function["arguments"])
                except (ValueError, TypeError):
                    args = None
                valid = name in tools_by_name and valid_args(tools_by_name[name], args)
                result = execute(files, name, args) if valid else {"ok": False, "error": "invalid tool request"}
                calls.append({"name": name, "args": args, "valid": valid, "ok": result["ok"]})
                messages.append({"role": "tool", "tool_call_id": call["id"],
                                 "content": json.dumps(result, separators=(",", ":"))})
    except (OSError, ValueError, KeyError, IndexError, RuntimeError) as exc:
        error = str(exc)
    scores = score(case, text, calls, files)
    if error:
        scores = dict.fromkeys(scores, False)
    return {"id": case["id"], "category": case["category"], "model": model, "variant": variant,
            "prompt": case["prompt"], "answer": text, "completions": completions, "calls": calls,
            "written": {name: value for name, value in files.items() if FIXTURE.get(name) != value},
            "seconds": round(time.monotonic() - started, 2), "error": error, **scores}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sockets = parser.add_mutually_exclusive_group(required=True)
    sockets.add_argument("--socket", type=Path, help="shared inference service socket")
    sockets.add_argument("--socket-dir", type=Path, help="one llama-server socket per model")
    parser.add_argument("--model", choices=MODELS, action="append")
    parser.add_argument("--variant", choices=("baseline", "prompt", "examples", "schema", "decode", "gated", "thinking", "required", "final"), default="baseline")
    parser.add_argument("--category", action="append")
    parser.add_argument("--case", action="append")
    parser.add_argument("--cache-prompt", action="store_true", help="reuse the prefix on disposable llama servers")
    parser.add_argument("--max-tokens", type=int, metavar="1–512", default=256)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    if not 1 <= args.max_tokens <= 512:
        parser.error("--max-tokens must be between 1 and 512")
    cases = json.loads((HERE / "cases.json").read_text())
    if args.category:
        cases = [case for case in cases if case["category"] in args.category]
    if args.case:
        cases = [case for case in cases if case["id"] in args.case]
    if not cases:
        parser.error("no cases selected")

    totals = defaultdict(lambda: dict.fromkeys(("cases", "correct", "format", "tools", "pass"), 0))
    lock = Lock()

    def run_model(model):
        path = args.socket or args.socket_dir / f"{model}.sock"
        for case in cases:
            row = run_case(path, model, args.variant, case, args.cache_prompt, args.max_tokens)
            with lock:
                print(model, case["id"], "PASS" if row["pass"] else "FAIL",
                      repr(row["answer"][:120]), f'{row["seconds"]}s', row["error"] or "", flush=True)
                print(json.dumps(row, ensure_ascii=False), file=output, flush=True)
                for category in (row["category"], "all"):
                    total = totals[f'{model}/{category}']
                    total["cases"] += 1
                    for key in ("correct", "format", "tools", "pass"):
                        total[key] += row[key]

    with args.output.open("x") as output, ThreadPoolExecutor(max_workers=1 if args.socket else 3) as pool:
        list(pool.map(run_model, args.model or MODELS))
    print(json.dumps(totals, indent=2))


if __name__ == "__main__":
    main()
