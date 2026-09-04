#!/usr/bin/env python3
"""Development-only cross-validation of an fs2iso image with pycdlib.

NOT part of the fs2iso deliverable (the tool itself is 100% Rust). Used during
development as an independent, strict ISO9660 reader: walks both the ISO9660
base tree and the Joliet tree of an image, and checks that the Joliet tree
lists exactly the payload files (paths + sizes) and that file contents read
back byte-identically through both namespaces.

Requires: pycdlib (pip install pycdlib)  -- dev environment only.

Usage:
    python verify_pycdlib.py <image.iso> <payload-dir>
"""

import io
import os
import sys

import pycdlib


def walk(iso, joliet):
    res = {}

    def rec(path):
        children = iso.list_children(joliet_path=path) if joliet else iso.list_children(iso_path=path)
        for child in children:
            ident = child.file_ident
            if ident in (b"\x00", b"\x01"):  # structural "." / ".."
                continue
            name = ident.decode("utf-16_be", "replace") if joliet else ident.split(b";")[0].decode("ascii", "replace")
            rel = name if path == "/" else path.lstrip("/") + "/" + name
            res[rel] = (child.isdir, child.data_length)
            if child.isdir:
                rec("/" + rel)

    rec("/")
    return res


def payload_list(root):
    out = {}
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort()
        for fn in sorted(filenames):
            full = os.path.join(dirpath, fn)
            rel = os.path.relpath(full, root).replace("\\", "/")
            out[rel] = os.path.getsize(full)
    return out


def main():
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
    image, payload = sys.argv[1], sys.argv[2]
    expected = payload_list(payload)

    iso = pycdlib.PyCdlib()
    try:
        iso.open(image)
    except Exception as e:  # noqa: BLE001 - report whatever the strict parser rejects
        print("FAIL: pycdlib could not open the image:", e)
        return 1

    jol = walk(iso, True)
    base = walk(iso, False)
    jol_files = {k: v[1] for k, v in jol.items() if not v[0]}

    missing = set(expected) - set(jol_files)
    extra = set(jol_files) - set(expected)
    size_bad = [k for k in expected if jol_files.get(k) != expected[k]]

    probes = sorted(expected)[:2]
    content_ok = True
    for rel in probes:
        jpath = "/" + rel
        buf = io.BytesIO()
        try:
            iso.get_file_from_iso_fp(buf, joliet_path=jpath)
            with open(os.path.join(payload, rel.replace("/", os.sep)), "rb") as f:
                content_ok = content_ok and buf.getvalue() == f.read()
        except Exception as e:  # noqa: BLE001
            content_ok = False
            print("  content probe", rel, "failed:", e)

    ok = (not missing) and (not extra) and (not size_bad) and content_ok
    print(f"image: {image}")
    print(f"  payload files: {len(expected)} | joliet files listed: {len(jol_files)}"
          f" | base entries: {len(base)}")
    print(f"  joliet names match: {not missing and not extra}"
          f" | missing: {sorted(missing)[:5]} | extra: {sorted(extra)[:5]}")
    print(f"  joliet sizes match: {not size_bad}")
    print(f"  content read-back OK (n={len(probes)}): {content_ok}")
    print("VERDICT:", "PASS" if ok else "FAIL")
    iso.close()
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
