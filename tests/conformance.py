"""Run the upstream ACP v2 compatibility kit against Elphie and a local model fixture."""

import http.server
import pathlib
import subprocess
import sys
import threading
import time

from terminal import Model


class DelayedModel(Model):
    def do_POST(self):
        time.sleep(0.1)
        super().do_POST()


def main():
    elphie = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/elphie").resolve()
    kit = sys.argv[2] if len(sys.argv) > 2 else "acp-tck"
    with http.server.ThreadingHTTPServer(("127.0.0.1", 0), DelayedModel) as server:
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            return subprocess.run([
                kit, "--protocol-version", "2", *sys.argv[3:], "--", str(elphie), "serve",
                "--provider", "ollama", "--model", "fixture",
                "--ollama-host", f"http://127.0.0.1:{server.server_port}",
            ]).returncode
        finally:
            server.shutdown()


if __name__ == "__main__":
    sys.exit(main())
