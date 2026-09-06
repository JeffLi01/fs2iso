#!/bin/bash
# QEMU + OVMF functional test for fs2iso (self-exit design).
#
# Method:
#   1. fs2iso packs payload + startup.nsh into the disc. The ESP (esp.img)
#      carries the user's files only — NO bootable .efi is needed: the Debian
#      OVMF used here has a BUILT-IN "EFI Internal Shell" (Boot Manager).
#   2. OVMF boots the disc alone; booting "fails" (esp.img has no boot
#      program -> "Not Found"), but the firmware STILL maps the El Torito
#      FAT image as an FS volume. We drive the Boot Manager with qemu
#      monitor sendkeys: ESC -> DOWN -> ENTER selects EFI Internal Shell.
#   3. The internal shell auto-runs startup.nsh from the ESP volume root
#      (EDK2 shells scan filesystem roots for startup.nsh); the script checks
#      payload files then writes the isa-debug-exit IO port via `mm`, which
#      makes qemu self-exit with code 1.
#   4. Verdict = process lifetime: exit 1 within the watchdog = PASS,
#      timeout/other = FAIL. No serial polling, no force-kill.
#
# Usage: bash tests/efi/run_acceptance.sh
# Env overrides: QEMU, ASSETS, FS2ISO, ISO, WATCHDOG (default 90)
set -eu
cd "$(dirname "$0")" || exit 1
W=$(cygpath -w "$PWD" | tr '\\' '/')
QEMU="${QEMU:-/c/Program Files/qemu/qemu-system-x86_64.exe}"
ASSETS="${ASSETS:-$W/assets}"
FS2ISO="${FS2ISO:-$W/../../target/release/fs2iso.exe}"
ISO="${ISO:-}"
WATCHDOG="${WATCHDOG:-90}"

CODE="$ASSETS/OVMF_CODE_4M.fd"
[ -f "$CODE" ] || CODE="$ASSETS/OVMF_CODE.fd"
VARS_SRC="$ASSETS/OVMF_VARS_4M.fd"
[ -f "$VARS_SRC" ] || VARS_SRC="$ASSETS/OVMF_VARS.fd"
[ -f "$CODE" ] && [ -f "$VARS_SRC" ] || {
  echo "missing OVMF in $ASSETS -- run tests/efi/fetch_assets.sh first"; exit 2
}
cp "$VARS_SRC" "$W/vars_run.fd"   # writable per-run copy

rm -rf payload out.iso serial.log
mkdir -p payload/tools
printf 'hello readme\n' > payload/readme.txt
printf 'nested payload\n' > payload/tools/nested.txt
# startup.nsh: no boot file needed anywhere — files-only payload, the ESP
# volume shows up as fs0 in this topology and the internal shell auto-runs
cat > payload/startup.nsh <<'NSH'
@echo -off
echo FS2ISO_TEST_START
fs0:
if exist readme.txt then
  echo FS_MOUNT_OK
  ls
  ls tools
  type readme.txt
  type tools\nested.txt
  mm -io 0x510 0x00 -w 2
endif
echo FS2ISO_TEST_NO_MOUNT_OR_READ
NSH

if [ -z "$ISO" ]; then
  "$FS2ISO" --flat -l FS2ISO out.iso payload
  ISO="$W/out.iso"
fi
echo "== image: $ISO =="
if ! grep -aq "ESP.IMG" "$ISO"; then
  echo "SKIP: image has no ESP (FAT) container — nothing for an EFI shell to mount."
  echo "(--no-eltorito data discs are validated by cargo test / pycdlib / Windows mount)"
  exit 0
fi

set +e
(
  sleep 15                                   # OVMF: boot fail -> Press any key
  echo "sendkey esc"                         # enter Boot Manager
  sleep 4
  echo "sendkey down"                        # highlight EFI Internal Shell
  sleep 1
  echo "sendkey ret"
  sleep 35                                   # internal shell boot + auto-run
  echo "quit"
) | timeout "$WATCHDOG" "$QEMU" -machine q35 \
  -drive if=pflash,format=raw,unit=0,file="$CODE",readonly=on \
  -drive if=pflash,format=raw,unit=1,file="$W/vars_run.fd" \
  -drive file="$ISO",format=raw,media=cdrom \
  -device isa-debug-exit,iobase=0x510,iosize=2 \
  -m 512 -display none -monitor stdio -serial file:serial.log -no-reboot \
  >qemu.out 2>qemu.err
RC=$?
set -e
rm -f "$W/vars_run.fd"
echo "== qemu exit code: $RC (watchdog=$WATCHDOG s) =="
tr -d '\000' < serial.log 2>/dev/null | grep -aE "FS2ISO_TEST|FS_MOUNT_OK|hello readme|nested payload|UEFI Interactive|mm:" | head -10 || true
if [ "$RC" = 1 ]; then
  echo "ACCEPTANCE PASS (self-exit code 1)"
  exit 0
elif [ "$RC" = 124 ]; then
  echo "ACCEPTANCE FAIL: watchdog timeout (internal shell not reached / volume not mounted)"
  exit 1
else
  echo "ACCEPTANCE FAIL: qemu exited with unexpected code $RC"
  exit 1
fi
