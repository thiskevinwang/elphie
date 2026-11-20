"""Exercise MCP discovery and tool calls through the installed TUI and ACP server."""

import http.server
import json
import os
import pathlib
import sys
import tempfile
import threading
from unittest.mock import patch

from terminal import Model, requests, terminal


def reply(request, source, log):
    method = request["method"]
    with log.open("a") as file:
        print(json.dumps({"source": source, "request": request}), file=file)
    if "id" not in request:
        return None
    response = {"jsonrpc": "2.0", "id": request["id"]}
    if method == "initialize":
        response["result"] = {"protocolVersion": "2025-11-25", "capabilities": {"tools": {}},
                              "serverInfo": {"name": source, "version": "1"}}
    elif method == "tools/list":
        page = request.get("params", {}).get("cursor")
        name = "lookup" if page == "next" else "unlisted"
        response["result"] = {"tools": [{
            "name": name, "description": "Look up a term",
            "inputSchema": {"type": "object", "properties": {"term": {"type": "string"}},
                            "required": ["term"]},
        }]}
        if page is None:
            response["result"]["nextCursor"] = "next"
    elif method == "tools/call":
        assert request["params"]["name"] == "lookup"
        assert request["params"]["arguments"] == {"term": "orbit"}
        response["result"] = {"content": [{"type": "text", "text": f"{source}_RESULT_742"}]}
    else:
        response["error"] = {"code": -32601, "message": "Method not found"}
    return response


def stdio_server(log):
    assert os.environ.get("ELPHIE_MCP_TEST_CHILD") == "fixture"
    assert "ELPHIE_MCP_TEST_TOKEN" not in os.environ
    for line in sys.stdin:
        response = reply(json.loads(line), "local", log)
        if response is not None:
            print(json.dumps(response), flush=True)


class HttpServer(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        if self.path != "/mcp" or self.headers.get("Authorization") != "Bearer token_fixture_583":
            return self.send_error(403)
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        response = reply(request, "remote", self.server.log)
        body = json.dumps(response).encode() if response is not None else b""
        self.send_response(200 if body else 202)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self.send_error(405)

    def log_message(self, *_):
        pass


class ToolModel(Model):
    def completion(self, body):
        if body["messages"][-1]["role"] == "tool":
            return {"role": "assistant", "content": "MCP_CHECK_COMPLETE"}
        calls = [{"function": {"name": name, "arguments": {"term": "orbit"}}}
                 for name in ("mcp_local__lookup", "mcp_remote__lookup", "mcp_local__unlisted")]
        return {"role": "assistant", "content": "", "tool_calls": calls}


def check(program):
    with tempfile.TemporaryDirectory() as scratch, http.server.ThreadingHTTPServer(
            ("127.0.0.1", 0), HttpServer) as httpd:
        root = pathlib.Path(scratch)
        httpd.log = root / "http.jsonl"
        child_log = root / "stdio.jsonl"
        config = root / "mcp.json"
        config.write_text(json.dumps({"servers": [
            {"name": "local", "type": "stdio", "command": sys.executable,
             "args": [str(pathlib.Path(__file__).resolve()), "--stdio-fixture", str(child_log)],
             "env": ["ELPHIE_MCP_TEST_CHILD"], "tools": ["lookup"]},
            {"name": "remote", "type": "http", "url": f"http://127.0.0.1:{httpd.server_port}/mcp",
             "bearerTokenEnv": "ELPHIE_MCP_TEST_TOKEN", "tools": ["lookup"]},
        ]}))
        with patch.dict(os.environ, {"ELPHIE_MCP_TEST_TOKEN": "token_fixture_583",
                                     "ELPHIE_MCP_TEST_CHILD": "fixture"}):
            listener = threading.Thread(target=httpd.serve_forever, daemon=True)
            try:
                with terminal(program, ToolModel, "--mcp-config", str(config)) as (fd, output):
                    listener.start()
                    output.expect("MCP local, remote", "? for shortcuts")
                    os.write(fd, b"look up orbit\r")
                    output.expect("MCP_CHECK_COMPLETE", "local / lookup · done", "remote / lookup · done",
                                  "failed", absent=("local_RESULT_742", "remote_RESULT_742", "token_fixture_583"))
            finally:
                if listener.is_alive():
                    httpd.shutdown()
        first, second = [requests.get(timeout=5)[1] for _ in range(2)]
        assert {t["function"]["name"] for t in first["tools"]} == {"mcp_local__lookup", "mcp_remote__lookup"}
        results = [m for m in second["messages"] if m["role"] == "tool"]
        assert len(results) == 3 and "local_RESULT_742" in results[0]["content"]
        assert "remote_RESULT_742" in results[1]["content"] and "Error" in results[2]["content"]
        assert "token_fixture_583" not in json.dumps(second)
        for path in (child_log, httpd.log):
            trace = [json.loads(line)["request"] for line in path.read_text().splitlines()]
            calls = [r["params"]["name"] for r in trace if r["method"] == "tools/call"]
            assert calls == ["lookup"], calls
            hello = next(r for r in trace if r["method"] == "initialize")
            info = hello["params"]["clientInfo"]
            assert info["name"].lower() == "elphie"
            assert info["icons"][0]["src"].startswith("data:image/webp;base64,")
    print("Passed terminal MCP discovery, stdio and HTTP calls, tool grants and credential isolation.")


if __name__ == "__main__":
    if sys.argv[1:2] == ["--stdio-fixture"]:
        stdio_server(pathlib.Path(sys.argv[2]))
    else:
        check(pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/elphie").resolve())
