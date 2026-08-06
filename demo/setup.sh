#!/usr/bin/env sh
# Build a throwaway git repo with a colorful, multi-file diff so the VHS tapes
# have something worth looking at. Idempotent: wipes and rebuilds each run.
set -eu

REPO="${1:-/tmp/drift-demo}"
rm -rf "$REPO"
mkdir -p "$REPO"
cd "$REPO"
git init -q
git config user.name drift
git config user.email drift@example.com
git config drift.theme dracula   # non-ansi theme => syntax highlighting on

# --- initial commit -------------------------------------------------------
cat > server.py <<'PY'
import json
from http.server import BaseHTTPRequestHandler, HTTPServer


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        body = {"status": "ok", "items": [1, 2, 3]}
        self.wfile.write(json.dumps(body).encode())


def main():
    server = HTTPServer(("0.0.0.0", 8000), Handler)
    server.serve_forever()
PY

cat > utils.js <<'JS'
export function greet(name) {
  return "Hello, " + name;
}

export function sum(xs) {
  let total = 0;
  for (const x of xs) total += x;
  return total;
}
JS

cat > README.md <<'MD'
# demo

A tiny service used to show off drift.
MD

git add -A
git commit -qm "initial service"

# --- working-tree changes drift will render -------------------------------
cat > server.py <<'PY'
import json
import logging
from http.server import BaseHTTPRequestHandler, HTTPServer

log = logging.getLogger("demo")


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        body = {"status": "ready", "items": [1, 2, 3, 5, 8]}
        self.wfile.write(json.dumps(body).encode())

    def log_message(self, fmt, *args):
        log.info(fmt, *args)


def main():
    logging.basicConfig(level=logging.INFO)
    server = HTTPServer(("0.0.0.0", 8080), Handler)
    log.info("listening on :8080")
    server.serve_forever()
PY

cat > utils.js <<'JS'
export function greet(name, greeting = "Hello") {
  return `${greeting}, ${name}!`;
}

export function sum(xs) {
  return xs.reduce((total, x) => total + x, 0);
}

export function unique(xs) {
  return [...new Set(xs)];
}
JS

# a brand-new untracked file, surfaced with `drift -A`
cat > CHANGELOG.md <<'MD'
# Changelog

## Unreleased
- structured logging
- template-string greeting
MD
