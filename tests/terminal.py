"""Exercise the built terminal and server with a local Ollama fixture on Unix."""

import codecs
import errno
import fcntl
import http.server
import json
import os
import pathlib
import pty
import queue
import re
import select
import signal
import struct
import sys
import termios
import threading
import time
import unicodedata


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
    # Ratatui sends cell edits with absolute cursor positions. Reassemble those
    # edits so a wait observes the visible screen, even across partial writes.
    csi = re.compile(r"\x1b\[([0-?]*)([ -/]*)([@-~])")

    def __init__(self, fd):
        self.fd = fd
        self.decoder = codecs.getincrementaldecoder("utf8")()
        self.pending = ""
        self.controls = set()
        self.resize(30, 90)

    def resize(self, height, width):
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
        self.height, self.width = height, width
        self.clear()

    def clear(self):
        self.cells = [[" "] * self.width for _ in range(self.height)]
        self.row = self.col = 0

    def feed(self, block):
        s = self.pending + self.decoder.decode(block)
        i = 0
        while i < len(s):
            if s[i] == "\x1b":
                command = self.csi.match(s, i)
                if not command:
                    break
                args, _, op = command.groups()
                if op in "Hf":
                    row, _, col = args.partition(";")
                    self.row, self.col = int(row or 1) - 1, int(col or 1) - 1
                elif op == "J" and args == "2":
                    self.clear()
                elif command[0] in ("\x1b[>1u", "\x1b[<1u", "\x1b[?1049l"):
                    self.controls.add(command[0])
                i = command.end()
                continue
            ch = s[i]
            if ch == "\r":
                self.col = 0
            elif ch == "\n":
                self.row += 1
            elif ch >= " ":
                if 0 <= self.row < self.height and 0 <= self.col < self.width:
                    self.cells[self.row][self.col] = ch
                self.col += 2 if unicodedata.east_asian_width(ch) in ("W", "F") else 1
            i += 1
        self.pending = s[i:]

    def expect(self, *present, absent=()):
        deadline = time.monotonic() + 10
        while True:
            screen = "\n".join("".join(row).rstrip() for row in self.cells)
            if all(text in screen or text in self.controls for text in present) and all(
                    text not in screen for text in absent):
                return
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([self.fd], [], [], remaining)[0]:
                raise AssertionError(f"Expected {present!r}, absent {absent!r}. Screen:\n{screen}")
            block = os.read(self.fd, 65536)
            if not block:
                raise AssertionError("Terminal closed before the expected screen appeared")
            self.feed(block)


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
        output = TerminalOutput(fd)
        expect = output.expect
        expect("\x1b[>1u", "Elphie", "fixture", "Ask Elphie", "? for shortcuts")

        os.write(fd, b"?")
        expect("Keyboard shortcuts", "Shift+Enter")
        os.write(fd, b"\x1b")
        expect("? for shortcuts", absent=("Keyboard shortcuts",))
        os.write(fd, b"helo\x1b[Dl\r")
        expect("FIRST_REPLY", "? for shortcuts")
        path, first = requests.get(timeout=5)
        assert (path, first) == ("/api/chat", {"model": "fixture", "stream": False,
                                              "messages": [{"role": "user", "content": "hello"}]})

        os.write(fd, b"/")
        expect("/model", "Select")
        os.write(fd, b"model")
        expect("Select a model", "fixture (current)", "orchid")
        os.write(fd, b"/absent\r")
        expect("No matching models")
        os.write(fd, b"\x1b")
        expect("fixture (current)", "orchid", absent=("No matching models",))
        os.write(fd, b"/OrC")
        expect("OrC", "orchid", absent=("fixture (current)",))
        output.resize(9, 36)
        expect("OrC", "orchid")
        output.resize(30, 90)
        expect("OrC", "orchid", absent=("fixture (current)",))
        os.write(fd, b"\r")
        expect("orchid.", "? for shortcuts")

        os.write(fd, b"line one\x1b[13;2uline two\x1b\rline three\nline four!")
        expect("line one", "line two", "line three", "line four!▏")
        assert requests.empty(), "A line break submitted the unfinished draft"
        os.write(fd, b"\r")
        expect("NEXT_REPLY", "? for shortcuts")
        _, second = requests.get(timeout=5)
        multiline = "line one\nline two\nline three\nline four!"
        assert second["model"] == "orchid", second
        assert second["messages"] == [*first["messages"], {"role": "assistant", "content": "FIRST_REPLY"},
                                      {"role": "user", "content": multiline}]

        os.write(fd, b"\x1b[A")
        expect("line four!▏")
        os.write(fd, b"?\r")
        _, recalled = requests.get(timeout=5)
        assert recalled["messages"] == [*second["messages"], {"role": "assistant", "content": "NEXT_REPLY"},
                                        {"role": "user", "content": multiline + "?"}]
        expect("line four!?", "? for shortcuts")
        os.write(fd, b"/m\t")
        expect("orchid (current)")
        os.write(fd, b"\x1b[A\x1b")
        expect("Cancelled")
        os.write(fd, b"hold\r")
        _, held = requests.get(timeout=5)
        assert held["model"] == "orchid" and held["messages"][-1]["content"] == "hold", held
        expect("Working", absent=("Cancelled",))
        os.write(fd, b"\x1b")
        expect("Cancelled")
        os.write(fd, b"\x03")
        expect("\x1b[<1u", "\x1b[?1049l")
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
        print("Passed: shortcuts, model search and selection, resize, multiline recall, cancellation, clean exit.")
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
