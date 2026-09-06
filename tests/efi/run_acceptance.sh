#!/bin/bash
# QEMU + OVMF + EFI Shell acceptance test for fs2iso.
#
# Proves the core user scenario: a UEFI (EDK2) shell must be able to see and
# read the media fs2iso produces when it is attached as a virtual CD/DVD.
# (Pure-ISO9660 data CDs are NOT mountable by EDK2 shells -- only the UDF
# side of the bridge is -- so this is the decisive functional gate.)
#
# Usage:
#   bash tests/efi/run_acceptance.sh            # builds release + fixture
# Env overrides: FS2ISO (binary), QEMU (qemu-system-x86_64), ASSETS (dir),
#                ISO (pre-built image to test instead of building one)
set -eu
cd "$(dirname "$0")" || exit 1
W=$(cygpath -w "$PWD" | tr '\\' '/')   # windows-style paths for native qemu

QEMU="${QEMU:-/c/Program Files/qemu/qemu-system-x86_64.exe}"
ASSETS="${ASSETS:-$W/assets}"
FS2ISO="${FS2ISO:-$PWD/../../target/release/fs2iso.exe}"
ISO="${ISO:-}"

CODE="$ASSETS/OVMF_CODE_4M.fd"
[ -f "$CODE" ] || CODE="$ASSETS/OVMF_CODE.fd"
VARS="$ASSETS/OVMF_VARS_4M.fd"
[ -f "$VARS" ] || VARS="$ASSETS/OVMF_VARS.fd"
SHELL="$ASSETS/shellx64.efi"
[ -f "$SHELL" ] || SHELL=$(find "$ASSETS" -iname '*.efi' | head -1)
for f in "$CODE" "$VARS" "$SHELL"; do
  [ -f "$f" ] || { echo "missing $f -- run tests/efi/fetch_assets.sh first"; exit 2; }
done

rm -rf work payload out.iso serial.log
mkdir -p work/EFI/BOOT payload/tools/bmc
cp "$SHELL" work/EFI/BOOT/BOOTX64.EFI
printf 'hello readme\n' > payload/readme.txt
printf 'dot file\n' > payload/.hidden
printf 'chinese-content\n' > 'payload/固件更新 2024.txt'
printf 'nested payload\n' > payload/tools/bmc/nested.txt
head -c 5000 /dev/urandom > payload/blob.dat

if [ -z "$ISO" ]; then
  "$FS2ISO" --flat -l fs2iso-udf out.iso payload
  ISO="$W/out.iso"
fi

cat > work/startup.nsh <<'NSH'
@echo -off
echo SHELL_STARTED_OK
map -r
echo FS_SCAN_BEGIN
ls fs1:\
echo ---
ls fs1:\tools\bmc
echo CONTENT_BEGIN
type fs1:\readme.txt
type fs1:\tools\bmc\nested.txt
echo CONTENT_END
echo ACCEPT_END
NSH

"$QEMU" -machine q35 \
  -drive if=pflash,format=raw,unit=0,file="$CODE",readonly=on \
  -drive if=pflash,format=raw,unit=1,file="$VARS" \
  -drive file=fat:rw:"$W/work",format=raw \
  -drive file="$ISO",format=raw,media=cdrom \
  -m 256 -display none -serial file:serial.log -monitor none -no-reboot \
  >qemu.out 2>qemu.err &
QPID=$!
for i in $(seq 1 60); do
  sleep 2
  grep -q "ACCEPT_END" serial.log 2>/dev/null && break
done
powershell.exe -NoProfile -Command "Stop-Process -Id $QPID -Force -ErrorAction SilentlyContinue" >/dev/null 2>&1 || true

LOG=$(tr -d '\000' < serial.log 2>/dev/null || true)
echo "$LOG" | grep -aE "readme\.txt|\.hidden|固件更新|blob\.dat|nested\.txt|update\.nsh" | head -12
PASS=1
echo "$LOG" | grep -q "hello readme" || { echo "FAIL: readme.txt content not readable"; PASS=0; }
echo "$LOG" | grep -q "nested payload" || { echo "FAIL: tools/bmc/nested.txt content not readable"; PASS=0; }
echo "$LOG" | grep -q "blob.dat" || { echo "FAIL: blob.dat not listed"; PASS=0; }
echo "$LOG" | grep -q "固件更新 2024" || { echo "WARN: chinese filename not in listing (serial charset)"; }
if [ "$PASS" = 1 ]; then echo "ACCEPTANCE PASS"; else echo "ACCEPTANCE FAIL"; exit 1; fi
