#!/bin/bash
# Fetch the OVMF firmware (edk2 build, Debian ovmf-generic) for the QEMU
# acceptance test. No external EFI Shell is needed: this OVMF carries a
# built-in "EFI Internal Shell" in its Boot Manager.
# Output: tests/efi/assets/{OVMF_CODE_4M.fd, OVMF_VARS_4M.fd}
set -eu
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)"
cd "$SCRIPT_DIR" || exit 1
OUT=assets
rm -rf "$OUT"; mkdir -p "$OUT" x
POOL="https://deb.debian.org/debian/pool/main/e/edk2"

curl -sSL -m 120 "$POOL/" -o dir.html
GEN=$(grep -oE 'ovmf-generic_[0-9][^"<]*_all\.deb' dir.html | sort -uV | tail -1)
echo "ovmf: $GEN"
curl -sSL -m 300 -o ovmf.deb "$POOL/$GEN"

# extract .ar -> data.tar.xz -> files (python: ar + tarfile/lzma)
if command -v python3 >/dev/null 2>&1; then
    PYTHON=(python3)
elif command -v python >/dev/null 2>&1; then
    PYTHON=(python)
elif command -v py >/dev/null 2>&1; then
    PYTHON=(py -3)
else
    echo "Python 3 not found (install python3/python or set up the Windows py launcher)"
    exit 2
fi

"${PYTHON[@]}" - <<'PYEOF'
import io, os, tarfile, sys

def members(deb):
    data = open(deb, 'rb').read()
    assert data[:8] == b'!<arch>\n'
    pos = 8
    while pos < len(data):
        hdr = data[pos:pos+60]
        name = hdr[:16].decode().strip()
        size = int(hdr[48:58].decode().strip())
        body = data[pos+60:pos+60+size]
        if name == 'data.tar.xz':
            yield body
        pos += 60 + size + (2 if size % 2 else 0)

for body in members('ovmf.deb'):
    tf = tarfile.open(fileobj=io.BytesIO(body), mode='r:xz')
    for m in tf:
        if not m.isfile():
            continue
        base = os.path.basename(m.name)
        if base in ('OVMF_CODE_4M.fd', 'OVMF_VARS_4M.fd', 'OVMF_CODE.fd', 'OVMF_VARS.fd'):
            out = os.path.join('assets', base)
            with open(out, 'wb') as f:
                f.write(tf.extractfile(m).read())
            print('extracted', base)
PYEOF
rm -rf x dir.html ovmf.deb
ls -la "$OUT"
echo "assets ready"
