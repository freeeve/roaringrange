#!/usr/bin/env python3
"""Static file server that honors HTTP Range requests — required by the roaringrange reader.

Python's stock ``http.server`` ignores ``Range`` and replies ``200`` with the whole body; the
wasm reader (``WasmFetch``) demands exactly the requested byte count and errors otherwise, so the
demo will not load under ``python3 -m http.server``. This subclass adds single-range support
(``206 Partial Content`` with ``Content-Range``) so the split files, manifest, ``.rrhc`` bundle,
and record store can be range-fetched.

    python3 serve.py [PORT]      # default 8080
"""

import os
import re
import sys
from http.server import HTTPServer, SimpleHTTPRequestHandler


class RangeHandler(SimpleHTTPRequestHandler):
    """Serves a single byte range per request; falls back to the default full-body send."""

    extensions_map = {**SimpleHTTPRequestHandler.extensions_map, ".wasm": "application/wasm"}

    def do_GET(self):
        rng = self.headers.get("Range")
        path = self.translate_path(self.path)
        if rng is None or not os.path.isfile(path):
            return super().do_GET()

        m = re.fullmatch(r"bytes=(\d+)-(\d*)", rng.strip())
        if not m:
            return super().do_GET()

        size = os.path.getsize(path)
        start = int(m.group(1))
        end = int(m.group(2)) if m.group(2) else size - 1
        end = min(end, size - 1)
        if start > end:
            self.send_response(416)
            self.send_header("Content-Range", f"bytes */{size}")
            self.end_headers()
            return

        length = end - start + 1
        self.send_response(206)
        self.send_header("Content-Type", self.guess_type(path))
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        self.send_header("Content-Length", str(length))
        self.end_headers()
        with open(path, "rb") as f:
            f.seek(start)
            self.wfile.write(f.read(length))


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8080
    print(f"serving {os.getcwd()} on http://localhost:{port}/ (Range-aware)")
    HTTPServer(("", port), RangeHandler).serve_forever()
