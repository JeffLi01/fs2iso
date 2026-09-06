# QEMU + OVMF + EFI Shell acceptance environment

Real-firmware functional gate for the core user scenario:
*attach the fs2iso output to a running UEFI system, enter the EFI shell,
and see/read the files* (BMC virtual media use case).

## Why this exists

EDK2-based firmware (e.g. the Debian/Ubuntu OVMF, and any EDK2-lineage
server firmware/shell) ships **no ISO9660 file-system driver**: a plain
ISO9660 data CD shows up as `BLKx` only — never as `fsX:` — no matter how
spec-conformant the image is. Such firmware mounts the **UDF** side of a
UDF-bridge disc instead. fs2iso therefore emits a UDF bridge
(ISO9660 + Joliet + UDF over the same file data), and this harness verifies
that an EDK2 shell really can mount and read it.

Verified behaviour matrix (QEMU 11 + Debian OVMF/efi-shell 2026.05):

| media | EDK2 shell result |
|---|---|
| pure ISO9660 (any producer) | `BLK` only, no `fsX`, files invisible |
| UDF bridge (fs2iso output) | `fs1:` mounted; names, subdirs and contents readable |

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

The script builds a fixture payload, packs it with fs2iso, boots OVMF from a
FAT drive into the real EFI shell, attaches the ISO as a SATA CD, runs
`map -r` / `ls` / `type` from `startup.nsh`, and prints `ACCEPTANCE PASS`
(or FAIL with a reason). Exit code 0 = pass.

Env overrides: `FS2ISO` binary path, `QEMU` binary path, `ASSETS` dir,
`ISO` to test a pre-built image instead of building one.

## Notes / limitations (verified)

- El Torito *direct-.efi* boot entries are not bootable under EDK2 (OVMF
  prints "failed to load … Not Found"); EDK2 bootable CDs use the FAT-ESP
  image flavour. Data-CD visibility — the BMC shell scenario — is unaffected
  and is what this harness gates.
- Serial console is charset-limited: Chinese file *names* list fine; file
  *contents* are validated with ASCII fixtures here.
