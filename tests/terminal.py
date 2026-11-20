"""Exercise the built terminal and server with a local Ollama fixture on Unix."""

import errno
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
    def do_GET(self):
        if self.path != "/api/tags":
            return self.send_error(404)
        self.respond({"models": [{"name": "fixture"}, {"name": "orchid"}]})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if self.path == "/api/show":
            return self.respond({"capabilities": ["completion"]})
        if self.path != "/api/chat":
            return self.send_error(404)
        requests.put((self.path, body))
        prompt = body["messages"][-1]["content"]
        if prompt == "hold":
            release.wait(timeout=15)
        reply = "FIRST_REPLY" if prompt == "hello" else "NEXT_REPLY"
        self.respond({"done": True, "message": {"role": "assistant", "content": reply}})

    def respond(self, value):
        output = json.dumps(value).encode()
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


class TerminalOutput:
    def __init__(self, fd):
        self.fd = fd
        self.buffer = bytearray()

    def expect(self, phrase):
        deadline = time.monotonic() + 10
        while phrase not in self.buffer:
            if time.monotonic() >= deadline:
                raise AssertionError(f"Missing {phrase!r} in terminal output: {self.buffer[-2000:]!r}")
            if select.select([self.fd], [], [], 0.1)[0]:
                block = os.read(self.fd, 65536)
                if not block:
                    raise AssertionError(f"Terminal closed before {phrase!r}")
                self.buffer.extend(block)
        del self.buffer[:self.buffer.index(phrase) + len(phrase)]


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
        expect = TerminalOutput(fd).expect
        expect(b"\x1b[>1u")
        expect(b"Enter")
        os.write(fd, b"helo\x1b[Dl\r")
        expect(b"FIRST_REPLY")
        expect(b"Enter")
        path, first = requests.get(timeout=5)
        assert (path, first) == ("/api/chat", {"model": "fixture", "stream": False,
                                              "messages": [{"role": "user", "content": "hello"}]})
        os.write(fd, b"/")
        expect(b"Select")
        os.write(fd, b"model")
        expect(b"orchid")
        os.write(fd, b"\x1b[B\r")
        expect(b"orchid.")
        expect(b"Enter")
        os.write(fd, b"line one\x1b[13;2uline two\x1b\rline three\nline four!")
        expect("!▏".encode())
        assert requests.empty(), "A line break submitted the unfinished draft"
        os.write(fd, b"\r")
        expect(b"NEXT_REPLY")
        expect(b"Enter")
        _, second = requests.get(timeout=5)
        assert second["model"] == "orchid", second
        assert second["messages"] == [*first["messages"], {"role": "assistant", "content": "FIRST_REPLY"},
                                      {"role": "user", "content": "line one\nline two\nline three\nline four!"}]
        os.write(fd, b"/m\t")
        expect(b"orchid (current)")
        os.write(fd, b"\x1b[A\x1b")
        expect(b"Cancelled")
        os.write(fd, b"hold\r")
        _, third = requests.get(timeout=5)
        assert third["model"] == "orchid", third
        os.write(fd, b"\x1b")
        expect(b"Cancelled")
        os.write(fd, b"\x03")
        expect(b"\x1b[<1u")
        expect(b"\x1b[?1049l")
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            child, status = os.waitpid(pid, os.WNOHANG)
            if child:
                pid = None
                assert os.waitstatus_to_exitcode(status) == 0, status
                break
            if select.select([fd], [], [], 0.05)[0]:
                try:
                    os.read(fd, 65536)
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
        else:
            raise AssertionError("Elphie did not exit after Ctrl+C")
        print("Passed: immediate model selection, multiline prompt, history, cancellation, terminal restoration, clean exit.")
    finally:
        release.set()
        os.close(fd)
        if pid:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
