"""Serve declared development assets with Bazel's Python interpreter."""

import argparse
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("root")
    parser.add_argument("--port", type=int, default=4173)
    parser.add_argument("--bind", default="127.0.0.1")
    args = parser.parse_args()
    root = Path(args.root).resolve()
    handler = partial(SimpleHTTPRequestHandler, directory=str(root))
    print(f"Serving {root} at http://{args.bind}:{args.port}", flush=True)
    with ThreadingHTTPServer((args.bind, args.port), handler) as server:
        server.serve_forever()


if __name__ == "__main__":
    main()
