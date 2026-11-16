#!/usr/bin/env python3
"""Generate benchmark corpora for grapes.

Usage: python3 bench/gen.py OUT_DIR

Creates four corpora under OUT_DIR:
  few/    10 files x ~1 MB
  large/  4 files x ~250 MB
  flat/   50 dirs x 2000 files x ~8 KB (100k files, shallow)
  deep/   binary tree of depth 14, 2 files x ~8 KB per dir (~65k files)

Text is random lines of pseudo-words. Needles, for benchmark patterns:
  "zanzibar" - rare: ~50 lines per large file, ~0.5% of small files
  "ministry" - common word of the vocabulary (~1% of lines)
"""
import os
import random
import sys

rng = random.Random(1234)
SYL = ["ka", "lo", "mi", "ne", "ru", "ta", "shi", "po", "ve", "dan", "or", "el",
       "qui", "sto", "ba", "ex", "fin", "gra", "hum", "ji", "ly", "ost", "pre", "ul"]
VOCAB = ["".join(rng.choice(SYL) for _ in range(rng.randint(1, 4))) for _ in range(6000)]
VOCAB += ["the", "and", "of", "to", "in", "is", "for", "data", "error", "value"] * 40
VOCAB += ["ministry"] * 9 + ["Ministry"]  # ~1% of lines contain it


def make_pool(n):
    lines = []
    for _ in range(n):
        words = rng.choices(VOCAB, k=rng.randint(3, 16))
        if rng.random() < 0.15:
            words.insert(0, "    " * rng.randint(1, 3))
        lines.append(" ".join(words) + "\n")
    return lines


POOL = make_pool(200_000)
NEEDLE = "the Zanzibar code was here zanzibar\n"


def text(nbytes, needles=0):
    out, size = [], 0
    while size < nbytes:
        chunk = rng.choices(POOL, k=min(4096, nbytes // 60 + 1))
        out.extend(chunk)
        size += sum(len(c) for c in chunk)
    for _ in range(needles):
        out.insert(rng.randrange(len(out)), NEEDLE)
    return "".join(out)


def small_file(path):
    with open(path, "w") as f:
        f.write(text(4096, needles=1 if rng.random() < 0.005 else 0))


def main(out):
    d = os.path.join(out, "few")
    os.makedirs(d, exist_ok=True)
    for i in range(10):
        with open(os.path.join(d, f"f{i}.txt"), "w") as f:
            f.write(text(1 << 20, needles=1 if i % 3 == 0 else 0))

    d = os.path.join(out, "large")
    os.makedirs(d, exist_ok=True)
    for i in range(4):
        with open(os.path.join(d, f"big{i}.txt"), "w") as f:
            f.write(text(250 << 20, needles=50))

    for i in range(50):
        d = os.path.join(out, "flat", f"d{i:02}")
        os.makedirs(d, exist_ok=True)
        for j in range(2000):
            small_file(os.path.join(d, f"f{j:04}.txt"))

    def tree(path, depth):
        os.makedirs(path, exist_ok=True)
        for j in range(2):
            small_file(os.path.join(path, f"f{j}.txt"))
        if depth:
            tree(os.path.join(path, "a"), depth - 1)
            tree(os.path.join(path, "b"), depth - 1)

    tree(os.path.join(out, "deep"), 14)


if __name__ == "__main__":
    main(sys.argv[1])
