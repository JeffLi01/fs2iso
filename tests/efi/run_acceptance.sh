#!/bin/bash
# QEMU + OVMF + EFI Shell functional test for fs2iso's default output.
#
# fs2iso's disc is a bootable EFI-shell medium: the payload is packed BOTH
# into the ISO9660(+Joliet) data tree and into a FAT container (esp.img)
# which the El Torito entry loads at boot. EDK2 firmware always supports
# FAT, so the shell sees every payload file on the FAT volume after boot.
#
# Method (self-exit design):
#   1. fixture payload carries EFI/BOOT/BOOTX64.EFI (real shell), a
#      startup.nsh (checks files, then writes the QEMU isa-debug-exit IO
#      port via `mm`), plus marker files
#   2. fs2iso packs it all (--flat); OVMF boots the disc ALONE (no FAT drive)
#   3. verdict = process lifetime: exit code 1 within the watchdog = PASS,
#      timeout / other code = FAIL. No serial polling, no force-kill.
#
# Images without an ESP container (--no-eltorito data discs) cannot boot a
# shell under EDK2 (no ISO9660 data driver); the script reports SKIP for
# them — their functional gates are cargo test + pycdlib + Windows mount.
#
# Usage:
#   bash tests/efi/run_acceptance.sh
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
VARS="$ASSETS/OVMF_VARS_4M.fd"
[ -f "$VARS" ] || VARS="$ASSETS/OVMF_VARS.fd"
SHELL="$ASSETS/shellx64.efi"
[ -f "$SHELL" ] || SHELL=$(find "$ASSETS" -iname '*.efi' | head -1)
for f in "$CODE" "$VARS" "$SHELL"; do
  [ -f "$f" ] || { echo "missing $f -- run tests/efi/fetch_assets.sh first"; exit 2; }
done

rm -rf payload out.iso serial.log
mkdir -p payload/EFI/BOOT payload/tools
cp "$SHELL" payload/EFI/BOOT/BOOTX64.EFI
printf 'hello readme\n' > payload/readme.txt
printf 'nested payload\n' > payload/tools/nested.txt
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
  echo "SKIP: no FAT boot container (esp.img) in the image — EDK2 cannot boot"
  echo "a plain ISO9660 data disc. Functional gates for --no-eltorito output:"
  echo "cargo test, pycdlib, Windows Mount-DiskImage."
  exit 0
fi

set +e
timeout "$WATCHDOG" "$QEMU" -machine q35 \
  -drive if=pflash,format=raw,unit=0,file="$CODE",readonly=on \
  -drive if=pflash,format=raw,unit=1,file="$VARS" \
  -drive file="$ISO",format=raw,media=cdrom \
  -device isa-debug-exit,iobase=0x510,iosize=2 \
  -m 256 -display none -serial file:serial.log -monitor none -no-reboot \
  >qemu.out 2>qemu.err
RC=$?
set -e
echo "== qemu exit code: $RC (watchdog=$WATCHDOG s) =="
tr -d '\000' < serial.log 2>/dev/null | grep -aE "FS2ISO_TEST|FS_MOUNT_OK|hello readme|nested payload|mm:" | head -8 || true
if [ "$RC" = 1 ]; then
  echo "ACCEPTANCE PASS (self-exit code 1)"
  exit 0
elif [ "$RC" = 124 ]; then
  echo "ACCEPTANCE FAIL: watchdog timeout (media did not boot / startup.nsh not reached)"
  exit 1
else
  echo "ACCEPTANCE FAIL: qemu exited with unexpected code $RC"
  exit 1
fi
