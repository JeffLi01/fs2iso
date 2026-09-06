# QEMU + OVMF + EFI Shell functional test (self-exit design)

Real-firmware (EDK2/OVMF) test gate for fs2iso images, driven by the EFI
shell's auto-run of `startup.nsh` and QEMU's `isa-debug-exit` device.

## Method

1. The harness builds a payload that includes `startup.nsh`; **fs2iso packs it
   into the ISO under test** (startup.nsh lands at the ISO root).
2. OVMF boots a real EDK2 shell from a minimal FAT drive (shell only, no
   startup.nsh). The shell scans filesystem roots and **auto-runs the
   startup.nsh found on the ISO**.
3. The script mounts the CD (`fs1:`), checks a marker file and, on success,
   writes the debug-exit IO port with the shell's `mm` command:
   `mm -io 0x510 0x00 -w 2` (note: `-w` is in **bytes**; the address must be
   aligned to the access width — 0x501/16-bit fails, 0x510/16-bit works).
   QEMU then self-exits with code `(0<<1)|1 = 1`.
4. **Verdict from process lifetime alone**: exit code 1 within the watchdog
   timeout = PASS; timeout or any other code = FAIL. No serial polling, no
   force-kill.

## Verified format facts (QEMU 11 + Debian OVMF/efi-shell 2026.05, and host
## Windows 11 Mount-DiskImage control experiments)

| media | this EDK2 gate | Windows 11 mount |
|---|---|---|
| pure ISO9660 (fs2iso default, ISO9660+Joliet) | **SKIP** — EDK2 has no ISO9660 data driver, the media can never mount (timeout is the expected outcome, not an image bug) | ✅ mounts, files listed (CDFS; Explorer prefers Joliet originals) |
| UDF bridge (hadris-cd UDF enabled, git 9f17b72) | ✅ PASS (mounts, `type` reads, self-exit 1) | ❌ unreadable (udfs.sys rejects hadris-cd's incomplete UDF — missing ECMA-167 file-set terminator) |

No single namespace satisfies both an EDK2 shell and Windows' strict UDF
driver with hadris-cd's current UDF writer, hence the default output is
ISO9660+Joliet (Windows + mainstream AMI-class BMC firmware). This gate's
full run applies to UDF / bootable-ESP variants of the image.

## Setup

1. `bash tests/efi/fetch_assets.sh` — downloads Debian `ovmf-generic` and
   `efi-shell-x64` packages and extracts OVMF + Shell binaries into
   `tests/efi/assets/` (gitignored; ~7 MB).
2. qemu-system-x86_64 installed (tested with 11.x) and
   `cargo build --release`.

## Run

```bash
bash tests/efi/run_acceptance.sh              # default fs2iso output -> SKIP note
ISO=/path/to/udf_or_esp.iso bash tests/efi/run_acceptance.sh
```

Env overrides: `FS2ISO`, `QEMU`, `ASSETS`, `ISO`, `WATCHDOG` (s, default 90).
Serial log is kept at `tests/efi/serial.log` for diagnostics only — verdicts
never parse it.

## Notes

- EDK2 shell startup.nsh auto-run scans filesystem roots: FAT may hold only
  the shell; the startup.nsh on the ISO executes even though the shell booted
  from the FAT drive.
- El Torito *direct-.efi* boot entries are not bootable under EDK2 (OVMF:
  "failed to load … Not Found"); EDK2-bootable CDs need a FAT-ESP boot image.
- Serial charset limits Chinese display; content checks use ASCII fixtures.
