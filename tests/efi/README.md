# QEMU + OVMF + EFI Shell functional test

Real-firmware gate for fs2iso's default output — a **bootable EFI-shell
disc** (payload in the ISO9660 data tree AND in a FAT container `esp.img`
that El Torito loads at boot).

## Method (self-exit design)

1. Fixture payload: `EFI/BOOT/BOOTX64.EFI` (real EDK2 shell), `startup.nsh`
   (checks payload files, then writes the QEMU debug-exit port), marker files.
2. `fs2iso --flat` packs everything. OVMF boots **the disc alone** (no helper
   FAT drive). The firmware loads `esp.img` (FAT), the shell starts with the
   FAT volume as `fs0:` and auto-runs `startup.nsh` from its root — i.e. the
   shell reads the payload files.
3. `startup.nsh` ends with `mm -io 0x510 0x00 -w 2` → qemu's
   `isa-debug-exit` device → qemu self-exits with code 1.
4. **Verdict from process lifetime alone**: exit 1 within the watchdog =
   PASS; timeout or any other code = FAIL. No serial polling, no force-kill.

Known quirks (calibrated):
- EDK2 `mm -w` is in **bytes** (`-w 2` = 16-bit); address must be aligned to
  the width (`0x510` works, odd `0x501` errors).
- qemu self-exit via isa-debug-exit does NOT flush `-serial file:` (0 bytes);
  verdicts rely on the exit code only. Timeout (killed) runs DO keep the log,
  which covers debugging.
- A foreground qemu call must be guarded with `set +e`/`RC=$?`/`set -e` in a
  `set -e` script — exit code 1 (the PASS signal) would abort it otherwise.

## Verified facts (QEMU 11 + Debian OVMF/efi-shell 2026.05 + Windows 11)

| media | EDK2/OVMF (boots the disc) | Windows mount |
|---|---|---|
| fs2iso default (ISO9660+Joliet + FAT esp.img) | ✅ boots → shell → payload files readable on fs0 (PASS) | ✅ data tree lists payload + esp.img/boot.catalog artifacts |
| plain ISO9660 (--no-eltorito) | SKIP: EDK2 has no ISO9660 data driver; nothing to boot | ✅ data tree lists payload |
| UDF bridge (historical, git 9f17b72) | ✅ fsX mount | ❌ udfs.sys rejects hadris-cd's incomplete UDF |

## Setup & run

```bash
bash tests/efi/fetch_assets.sh    # OVMF + shell from Debian pool -> assets/
cargo build --release
bash tests/efi/run_acceptance.sh  # builds fixture, boots, expects exit 1
```

Env overrides: `FS2ISO`, `QEMU`, `ASSETS`, `ISO` (pre-built image),
`WATCHDOG` (s, default 90). Serial kept at `tests/efi/serial.log` for
diagnostics only.
