#!/usr/bin/env python3
"""Serve SPA: return index.html for all routes so client-side routing works."""
import http.server
import os
import socketserver

DIR = os.path.join(os.path.dirname(__file__), "..", "frontend", "dist")
PORT = int(os.environ.get("PORT", "8765"))


class SPAHandler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=DIR, **kwargs)

    def do_GET(self):
        path = self.translate_path(self.path)
        if os.path.isdir(path):
            path = os.path.join(path, "index.html")
        if not os.path.exists(path) or not os.path.isfile(path):
            self.path = "/index.html"
        return http.server.SimpleHTTPRequestHandler.do_GET(self)


if __name__ == "__main__":
    os.chdir(DIR)
    with socketserver.TCPServer(("", PORT), SPAHandler) as httpd:
        print(f"Serving at http://localhost:{PORT}")
        httpd.serve_forever()
