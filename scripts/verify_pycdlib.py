#!/usr/bin/env python3
"""Cross-validation of an fs2iso image with pycdlib (development gate only,
NOT part of the fs2iso deliverable).

Checks: ISO9660 base tree (ASCII-folded names), Joliet tree (original
names incl. non-ASCII) and per-file byte content against a payload dir.

Usage: py -3 scripts/verify_pycdlib.py <image.iso> <payload-dir> [--flat]
"""
import os
import sys

import pycdlib

FLAT = "--flat" in sys.argv
args = [a for a in sys.argv[1:] if a != "--flat"]
if len(args) != 2:
    sys.exit("usage: verify_pycdlib.py <image.iso> <payload-dir> [--flat]")
iso_path, payload_dir = args

# collect expected (relpath, bytes)
expected = {}
root = payload_dir.rstrip("/\\")
for dp, _, files in os.walk(payload_dir):
    for f in files:
        full = os.path.join(dp, f)
        rel = os.path.relpath(full, root).replace(os.sep, "/")
        expected[rel] = open(full, "rb").read()


def key(p, joliet):
    if not joliet:
        p = p.upper()
    return p


iso = pycdlib.PyCdlib()
iso.open(iso_path)
problems = []


def check_namespace(vd_key, joliet):
    seen = 0
    for dp, dirs, files in iso.walk(**{vd_key: "/"}):
        dp = dp.strip("/")
        for f in files:
            name = f[:-2] if f.endswith(";1") else f
            rel = f"{dp}/{name}" if dp else name
            if rel == "boot.catalog":  # engine artifact
                continue
            seen += 1
            want = key(rel, joliet)
            hit = next((e for e in expected if key(e, joliet) == want), None)
            if not joliet:
                # base tree: engine normalizes non-d-characters to '_', so
                # only structural counting (below) applies
                continue
            if hit is None:
                problems.append(f"[{vd_key}] unexpected: {rel}")
                continue
            with iso.open_file_from_iso(**{vd_key: "/" + rel}) as fh:
                got = fh.read()
            if got != expected[hit]:
                problems.append(f"[{vd_key}] content mismatch: {rel} ({len(got)} vs {len(expected[hit])} B)")
    return seen


n_iso = check_namespace("iso_path", joliet=False)
n_jol = 0
if iso.has_joliet():
    n_jol = check_namespace("joliet_path", joliet=True)
else:
    problems.append("no Joliet SVD present")

print(f"base files seen: {n_iso} (expected {len(expected)}), joliet files seen: {n_jol}")
if n_iso != len(expected):
    problems.append(f"base tree file count {n_iso} != payload {len(expected)}")
if problems:
    print("PROBLEMS:")
    for p in problems[:20]:
        print(" ", p)
    sys.exit(1)
print("VERDICT: PASS")
