#!/usr/bin/env python3
"""Development-only cross-validation of an fs2iso image with pycdlib.

NOT part of the fs2iso deliverable (the tool itself is 100% Rust). Used during
development as an independent, strict ISO9660 reader.

Walks the namespace pycdlib resolves (Joliet tree when present, otherwise the
ISO9660 base tree) and checks that every payload file is listed with the
right size and that a couple of files read back byte-identically. Names are
compared in the engine's rendering: original (Joliet) or ASCII-uppercased
(isobemak base tree, non-ASCII bytes pass through).

Requires: pycdlib (pip install pycdlib)  -- dev environment only.

Usage:
    python verify_pycdlib.py <image.iso> <payload-dir>
"""

import io
import os
import sys

import pycdlib


def has_joliet(image_path):
    with open(image_path, "rb") as f:
        data = f.read(64 * 2048)
    for lba in range(16, 40):
        sec = data[lba * 2048:(lba + 1) * 2048]
        if len(sec) < 2048 or sec[0] == 255:
            break
        if sec[1:6] == b"CD001" and sec[0] == 2:
            return True
    return False


def walk(iso, joliet):
    res = {}

    def rec(path):
        children = iso.list_children(joliet_path=path) if joliet else iso.list_children(iso_path=path)
        for child in children:
            ident = child.file_ident
            if ident in (b"\x00", b"\x01"):  # structural "." / ".."
                continue
            if joliet:
                name = ident.decode("utf-16_be", "replace")
            else:
                # engine stores raw UTF-8 with ASCII uppercased + ";version"
                name = ident.decode("utf-8", "replace").split(";")[0]
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
    expected_raw = payload_list(payload)

    iso = pycdlib.PyCdlib()
    try:
        iso.open(image)
    except Exception as e:  # noqa: BLE001 - report whatever the strict parser rejects
        print("FAIL: pycdlib could not open the image:", e)
        return 1

    joliet = has_joliet(image)
    expected = {k.upper(): v for k, v in expected_raw.items()} if not joliet else expected_raw
    listed = walk(iso, joliet)
    files = {k: v[1] for k, v in listed.items() if not v[0]}

    missing = set(expected) - set(files)
    extra = set(files) - set(expected)
    if not joliet:
        extra.discard("BOOT.CATALOG")  # engine artifact when El Torito enabled
    size_bad = [k for k in expected if files.get(k) != expected[k]]

    probes = sorted(expected)[:2]
    content_ok = True
    for rel in probes:
        buf = io.BytesIO()
        try:
            iso.get_file_from_iso_fp(buf, iso_path="/" + rel + ("" if joliet else ";1"))
            with open(os.path.join(payload, rel.lower().replace("/", os.sep)), "rb") as f:
                content_ok = content_ok and buf.getvalue() == f.read()
        except Exception as e:  # noqa: BLE001
            content_ok = False
            print("  content probe", rel, "failed:", e)

    ok = (not missing) and (not extra) and (not size_bad) and content_ok
    print(f"image: {image}")
    print(f"  namespace: {'Joliet' if joliet else 'ISO9660 base'} | payload files: {len(expected)}"
          f" | listed files: {len(files)} | base entries: {len(listed)}")
    print(f"  names match: {not missing and not extra}"
          f" | missing: {sorted(missing)[:5]} | extra: {sorted(extra)[:5]}")
    print(f"  sizes match: {not size_bad}")
    print(f"  content read-back OK (n={len(probes)}): {content_ok}")
    print("VERDICT:", "PASS" if ok else "FAIL")
    iso.close()
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
