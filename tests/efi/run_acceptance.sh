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
#      makes qemu self-exit with the dedicated code 85.
#   4. Verdict = process lifetime: exit 85 within the watchdog = PASS,
#      timeout/other = FAIL. No serial polling, no force-kill.
#
# Usage: bash tests/efi/run_acceptance.sh
# Env overrides: QEMU, ASSETS, FS2ISO, ISO, WATCHDOG (default 90)
set -eu
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)"
cd "$SCRIPT_DIR" || exit 1
case "$(uname -s 2>/dev/null || printf 'unknown')" in
  MINGW*|MSYS*|CYGWIN*)
    W=$(cygpath -w "$SCRIPT_DIR" | tr '\\' '/')
    FS2ISO_NAME="fs2iso.exe"
    ;;
  *)
    W="$SCRIPT_DIR"
    FS2ISO_NAME="fs2iso"
    ;;
esac
# QEMU discovery: honour an explicit QEMU= override, then probe PATH and the
# common install roots (Program Files, MSYS2 mingw64/ucrt64) — no hardcoded
# single location.
QEMU="${QEMU:-}"
if [ -z "$QEMU" ] || [ ! -x "$QEMU" ]; then
  QEMU=""
  for cand in \
    "/c/Program Files/qemu/qemu-system-x86_64.exe" \
    "/c/msys64/mingw64/bin/qemu-system-x86_64.exe" \
    "/c/msys64/ucrt64/bin/qemu-system-x86_64.exe" \
    "$(command -v qemu-system-x86_64 2>/dev/null || true)"
  do
    if [ -n "$cand" ] && [ -x "$cand" ]; then QEMU="$cand"; break; fi
  done
fi
[ -n "$QEMU" ] && [ -x "$QEMU" ] || {
  echo "qemu-system-x86_64 not found (install it or set QEMU=/path/to/qemu-system-x86_64.exe)"
  exit 2
}
echo "qemu: $QEMU"
ASSETS="${ASSETS:-$W/assets}"
FS2ISO="${FS2ISO:-$W/../../target/release/$FS2ISO_NAME}"
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

rm -rf payload out.iso
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
  mm -io 0x510 0x2a -w 2
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
  -m 512 -display none -monitor stdio -serial none -no-reboot \
  >qemu.out 2>qemu.err
RC=$?
set -e
rm -f "$W/vars_run.fd"
echo "== qemu exit code: $RC (watchdog=$WATCHDOG s) =="
if [ "$RC" = 85 ]; then
  echo "ACCEPTANCE PASS (self-exit code 85)"
  exit 0
elif [ "$RC" = 124 ]; then
  echo "ACCEPTANCE FAIL: watchdog timeout (internal shell not reached / volume not mounted)"
  exit 1
elif [ "$RC" = 1 ]; then
  echo "ACCEPTANCE FAIL: qemu exited with generic code 1 without the acceptance magic value"
  exit 1
else
  echo "ACCEPTANCE FAIL: qemu exited with unexpected code $RC"
  exit 1
fi
