#!/bin/bash
# QEMU + OVMF + EFI Shell functional test (self-exit design).
#
# Method (user-specified):
#   1. fs2iso packs payload + startup.nsh into the ISO under test; OVMF boots
#      a real EDK2 shell from a minimal FAT drive (shell only, no startup.nsh).
#   2. The shell auto-runs startup.nsh from the ISO root (EDK2 scans
#      filesystem roots). The script mounts the CD, checks a marker file and
#      finally writes the QEMU isa-debug-exit IO port via `mm`, which makes
#      qemu exit with code 1.
#   3. Verdict from process lifetime alone: exit code 1 within the watchdog
#      timeout = PASS; anything else / timeout = FAIL. No serial polling, no
#      force-kill.
#
# Edge case: default fs2iso output is ISO9660-only, which EDK2 shells cannot
# mount at all (no ISO9660 data driver) -- startup.nsh is never reached and
# the run times out. That is the documented firmware limitation, so for such
# images the script reports SKIP (exit 0) with an explanation; the functional
# gates for ISO9660-only media are cargo test + pycdlib + Windows mount.
#
# Usage:
#   bash tests/efi/run_acceptance.sh             # default fs2iso output
#   ISO=/path/image.iso bash tests/efi/run_acceptance.sh
# Env overrides: QEMU, ASSETS, FS2ISO, ISO, WATCHDOG (seconds, default 90)
set -eu
cd "$(dirname "$0")" || exit 1
W=$(cygpath -w "$PWD" | tr '\\' '/')
QEMU="${QEMU:-/c/Program Files/qemu/qemu-system-x86_64.exe}"
ASSETS="${ASSETS:-$W/assets}"
FS2ISO="${FS2ISO:-$W/../../target/release/fs2iso.exe}"
ISO="${ISO:-}"
WATCHDOG="${WATCHDOG:-90}"
DBG_PORT=0x510

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
cp "$SHELL" work/EFI/BOOT/BOOTX64.EFI   # FAT drive: shell only, NO startup.nsh

# payload + startup.nsh are packed into the ISO by fs2iso
printf 'hello iso payload\n' > payload/readme.txt
printf 'nested content\n' > payload/tools/bmc/nested.txt
cat > payload/startup.nsh <<NSH
@echo -off
echo FS2ISO_TEST_START
fs1:
if exist readme.txt then
  echo FS_MOUNT_OK
  ls
  type readme.txt
  mm -io $DBG_PORT 0x00 -w 2
endif
echo FS2ISO_TEST_NO_MOUNT_OR_READ
NSH

if [ -z "$ISO" ]; then
  "$FS2ISO" --flat -l FS2ISO out.iso payload
  ISO="$W/out.iso"
fi

echo "== image: $ISO =="
if ! grep -aq "NSR02" "$ISO"; then
  echo "SKIP: ISO9660-only image is not mountable by EDK2 shells (no ISO9660"
  echo "data driver). Functional gates for this format: cargo test, pycdlib,"
  echo "Windows Mount-DiskImage. UDF/ESP variants run the full test below."
  exit 0
fi

set +e
timeout "$WATCHDOG" "$QEMU" -machine q35 \
  -drive if=pflash,format=raw,unit=0,file="$CODE",readonly=on \
  -drive if=pflash,format=raw,unit=1,file="$VARS" \
  -drive file=fat:rw:"$W/work",format=raw \
  -drive file="$ISO",format=raw,media=cdrom \
  -device isa-debug-exit,iobase=0x510,iosize=2 \
  -m 256 -display none -serial file:serial.log -monitor none -no-reboot \
  >qemu.out 2>qemu.err
RC=$?
set -e
echo "== qemu exit code: $RC (timeout=$WATCHDOG s) =="
tr -d '\000' < serial.log 2>/dev/null | grep -aE "FS2ISO_TEST|FS_MOUNT_OK|hello iso payload|mm:" | head -8 || true
if [ "$RC" = 1 ]; then
  echo "ACCEPTANCE PASS (self-exit code 1)"
  exit 0
elif [ "$RC" = 124 ]; then
  echo "ACCEPTANCE FAIL: watchdog timeout (media not mounted / startup.nsh not executed)"
  exit 1
else
  echo "ACCEPTANCE FAIL: qemu exited with unexpected code $RC"
  exit 1
fi
