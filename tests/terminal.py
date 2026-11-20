"""Exercise the built terminal and server with a local Ollama fixture on Unix."""

import fcntl
import http.server
import json
import os
import pathlib
import pty
import queue
import select
import signal
import struct
import sys
import termios
import threading
import time


requests = queue.Queue()
release = threading.Event()


class Model(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        requests.put((self.path, body))
        prompt = body["messages"][-1]["content"]
        if prompt == "hold":
            release.wait(timeout=15)
        reply = "FIRST_REPLY" if prompt == "hello" else "NEXT_REPLY"
        output = json.dumps({"done": True, "message": {"role": "assistant", "content": reply}}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(output)))
        self.end_headers()
        try:
            self.wfile.write(output)
        except ConnectionError:
            pass

    def log_message(self, *_):
        pass


def expect(fd, phrase):
    output = bytearray()
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if select.select([fd], [], [], 0.1)[0]:
            block = os.read(fd, 65536)
            if not block:
                break
            output.extend(block)
            if phrase in output:
                return
    raise AssertionError(f"Missing {phrase!r} in terminal output: {output[-2000:]!r}")


def main():
    program = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/elphie").resolve()
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Model)
    port = server.server_address[1]
    pid, fd = pty.fork()
    if pid == 0:
        server.server_close()
        fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 90, 0, 0))
        os.execv(program, [str(program), "--provider", "ollama", "--model", "fixture",
                           "--ollama-host", f"http://127.0.0.1:{port}"])
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        expect(fd, b"Enter")
        os.write(fd, b"helo\x1b[Dl\r")
        expect(fd, b"FIRST_REPLY")
        path, first = requests.get(timeout=5)
        assert (path, first) == ("/api/chat", {"model": "fixture", "stream": False,
                                              "messages": [{"role": "user", "content": "hello"}]})
        os.write(fd, b"again\r")
        expect(fd, b"NEXT_REPLY")
        _, second = requests.get(timeout=5)
        assert second["messages"] == [*first["messages"], {"role": "assistant", "content": "FIRST_REPLY"},
                                      {"role": "user", "content": "again"}]
        os.write(fd, b"hold\r")
        requests.get(timeout=5)
        os.write(fd, b"\x1b")
        expect(fd, b"Cancelled")
        os.write(fd, b"\x03")
        expect(fd, b"\x1b[?1049l")
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            child, status = os.waitpid(pid, os.WNOHANG)
            if child:
                pid = None
                assert os.waitstatus_to_exitcode(status) == 0, status
                break
            time.sleep(0.05)
        else:
            raise AssertionError("Elphie did not exit after Ctrl+C")
        print("Passed: terminal startup, two turns with history, cancellation, terminal restoration, clean exit.")
    finally:
        release.set()
        if pid:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        os.close(fd)
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
