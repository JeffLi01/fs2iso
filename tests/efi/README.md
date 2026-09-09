# QEMU + OVMF functional test

Real-firmware gate for fs2iso's default output — a disc whose ESP (`esp.img`,
FAT) carries the user's files and whose ISO9660(+Joliet) data tree carries
them too. Verified property under test: **the EFI shell sees the payload on
the FAT volume that firmware maps from the El Torito image** — exactly how
UEFI OS install media behave (Windows sees the directory tree; EFI shells see
the FAT filesystem).

## Method (no external shell needed — OVMF has a built-in one)

The Debian `OVMF_CODE_4M.fd` used here ships a **built-in "EFI Internal
Shell"** (Boot Manager entry). Verified chain (all on this firmware):

1. fs2iso packs a files-only payload + `startup.nsh` (no bootable .efi at all)
   into the disc.
2. OVMF tries to boot the disc; booting "fails" (`Not Found` — esp.img has no
   boot program), **but the firmware still maps the El Torito FAT image as an
   FS volume**.
3. The harness drives the Boot Manager over the qemu monitor
   (`sendkey esc` → `down` → `ret`) to select **EFI Internal Shell**.
4. The internal shell auto-runs `startup.nsh` from the ESP volume root
   (EDK2 shells scan filesystem roots for startup.nsh). The script checks
   payload files, then `mm -io 0x510 0x2a -w 2` writes the QEMU
   `isa-debug-exit` port with the acceptance magic value `0x2a` -> qemu
   self-exits with code 85 (`(0x2a << 1) | 1`).
5. Verdict = process lifetime: exit 85 within the watchdog = PASS; timeout or
   any other exit code = FAIL. No serial polling, no force-kill.

## Verified facts (QEMU 11 + Debian OVMF/efi-shell 2026.05)

| media | Windows mount | EFI shell (OVMF) |
|---|---|---|
| fs2iso default (data tree + esp.img FAT) | directory tree (artifacts hidden; only user files listed) | esp.img FAT volume mapped as FS → user files visible (PASS) |
| plain ISO9660 (--no-eltorito) | directory tree | BLK only, no fsX (EDK2 has no ISO9660 data driver) |

No boot file is required in the payload for the shell to see the FAT volume —
the firmware maps the El Torito image regardless of boot success.

## Setup & run

```bash
bash tests/efi/fetch_assets.sh    # OVMF only (~5 MB) -> assets/
cargo build --release
bash tests/efi/run_acceptance.sh
```

Env overrides: `FS2ISO`, `QEMU`, `ASSETS`, `ISO`, `WATCHDOG` (s, default 90).
QEMU monitor output is redirected to `qemu.out`; QEMU errors are kept in
`qemu.err` for diagnostics.

Known quirks (calibrated):
- EDK2 `mm -w` is in **bytes**; the debug-exit port address must be aligned
  to the access width (`0x510`/`-w 2` works; odd `0x501` errors).
- The acceptance verdict does not depend on serial output. The dedicated
   isa-debug-exit code 85 avoids treating a generic qemu exit code 1 as a pass.
   Timeout (killed) runs keep `qemu.out` and `qemu.err` for diagnostics.
- Boot-Manager navigation sleeps are topology/timing dependent; the watchdog
   absorbs jitter.
