#!/usr/bin/env python3
"""Wall-clock benchmark of grapes vs ripgrep vs GNU grep.

Usage: python3 bench/run.py DATA_DIR [GRAPES_BIN] [RUNS]

Each tool's stdout goes to a pipe that this script drains (so no tool can
take a /dev/null shortcut). Output lines are compared as sorted multisets so
the tools can be checked against each other regardless of file order.
"""
import hashlib
import os
import statistics
import subprocess
import sys
import time

DATA = sys.argv[1]
GRAPES = sys.argv[2] if len(sys.argv) > 2 else "target/release/grapes"
RUNS = int(sys.argv[3]) if len(sys.argv) > 3 else 5

CORPORA = ["few", "large", "flat", "deep"]
PATTERNS = {"rare": "zanzibar", "common": "ministry", "regex": r"err\w*\s+\w+ing"}


def tools(path, pat):
    return {
        "grapes": [GRAPES, "-n", "--color=never", "-d", path, pat],
        "rg": ["rg", "-n", "-i", "--no-ignore", "--hidden", "--color=never",
               "--no-heading", "--with-filename", pat, path],
        "grep": ["/usr/bin/grep", "-r", "-n", "-i", "--color=never", "-E", pat, path],
    }


def run(cmd):
    t0 = time.perf_counter()
    p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    lines = p.stdout.read().splitlines()
    p.wait()
    dt = time.perf_counter() - t0
    digest = hashlib.sha1(b"\n".join(sorted(lines))).hexdigest()[:10]
    return dt, len(lines), digest, p.returncode


def main():
    print(f"{'corpus':7} {'pattern':7} {'tool':7} {'median':>8} {'min':>8} {'lines':>8}  digest")
    for corpus in CORPORA:
        path = os.path.join(DATA, corpus)
        for pname, pat in PATTERNS.items():
            digests = {}
            for tool, cmd in tools(path, pat).items():
                run(cmd)  # warm-up (page cache)
                results = [run(cmd) for _ in range(RUNS)]
                times = [r[0] for r in results]
                _, n, digest, _ = results[0]
                digests[tool] = digest
                print(f"{corpus:7} {pname:7} {tool:7} {statistics.median(times):8.3f} "
                      f"{min(times):8.3f} {n:8}  {digest}", flush=True)
            if len(set(digests.values())) != 1:
                print(f"  !! outputs differ: {digests}")


if __name__ == "__main__":
    main()
