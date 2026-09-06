# QEMU + OVMF + EFI Shell acceptance environment

Real-firmware test gate for fs2iso's optical images under an EDK2 (OVMF)
UEFI firmware — the environment that exposed the format-level facts below.

## Verified format facts (QEMU 11 + Debian OVMF/efi-shell 2026.05, and host
## Windows 11 Mount-DiskImage control experiments)

| media | EDK2/OVMF shell | Windows 11 mount |
|---|---|---|
| pure ISO9660 (fs2iso default, ISO9660+Joliet) | optical drive **detected**, no `fsX` (EDK2 has no ISO9660 data driver) | ✅ mounts, files listed (base names via CDFS; Explorer prefers Joliet originals) |
| UDF bridge (hadris-cd UDF enabled, git 9f17b72) | `fs1:` mounts, contents readable (UdfDxe) | ❌ drive appears but volume unreadable (udfs.sys rejects hadris-cd's incomplete UDF — missing ECMA-167 file-set terminator) |

Conclusion: no single namespace satisfies both a UDF-only EDK2 shell and a
strict Windows UDF driver with hadris-cd's current UDF writer. Default output
is ISO9660+Joliet (Windows + mainstream AMI-class BMC firmware). The UDF
bridge variant is preserved in git history (`9f17b72`) for EDK2-only
firmware; revisit when the UDF writer becomes spec-complete.

## Setup

1. `bash tests/efi/fetch_assets.sh` — downloads Debian `ovmf-generic` and
   `efi-shell-x64` packages and extracts OVMF + Shell binaries into
   `tests/efi/assets/` (gitignored; ~7 MB).
2. Have qemu-system-x86_64 installed (tested with 11.x), and build fs2iso:
   `cargo build --release`.

## Run

```bash
bash tests/efi/run_acceptance.sh
```

Builds a fixture payload, packs it with fs2iso, boots OVMF from a FAT drive
into the real EFI shell, attaches the ISO as a SATA CD and runs
`map -r` / `ls` / `type` from `startup.nsh`. Assertions adapt to the image:

- image contains UDF (`NSR02` marker) → expect `fsX` mount and readable content
- ISO9660-only → expect the optical drive to be **detected** (`BLK`/DVD in
  the map); content-level equivalence is covered by `cargo test`, pycdlib and
  the Windows mount check

Exit code 0 = pass. Env overrides: `FS2ISO`, `QEMU`, `ASSETS`, `ISO`.

## Windows mount check (host, optional)

`scripts/` has no Windows-mount helper committed; the control procedure is:
mount a known-good ISO first (Nero fixture or any older fs2iso output); if it
also fails the host CD stack is wedged (ghost drive letters / ShellHWDetection
service) — reboot, then retest. Use `Mount-DiskImage` + `Get-ChildItem` on
the resulting letter.

## Notes

- El Torito *direct-.efi* boot entries are not bootable under EDK2 (OVMF
  prints "failed to load … Not Found"); EDK2 bootable CDs use the FAT-ESP
  image flavour. Data-CD visibility is unaffected.
- Serial console is charset-limited: Chinese file *names* list fine; content
  checks use ASCII fixtures.
