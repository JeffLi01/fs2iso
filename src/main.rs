//! fs2iso CLI — pack files/directories into an ISO9660 image for BMC virtual
//! media / UEFI shell (fs0) use. Pure Rust, zero dependencies.

use fs2iso::{build_iso, Options};
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

const HELP: &str = "\
fs2iso — pack files/directories into an ISO 9660 image (BMC virtual media / UEFI shell)

USAGE:
    fs2iso [OPTIONS] <OUTPUT.iso> <PATH>...

  Each PATH is packed under its own name at the ISO root:
    file   -> a file at the root
    dir    -> a directory (whole subtree) at the root
  With --flat, a directory's *contents* are merged into the root instead.

OPTIONS:
    -l, --label <NAME>     volume label (default: derived from OUTPUT file name)
        --flat             pack directory contents into the ISO root (mkisofs style)
        --boot-efi <FILE>  use payload FILE as the El Torito EFI boot image
                           (auto-detected: EFI/BOOT/BOOTX64.EFI unless --no-eltorito)
        --no-eltorito      do not add an El Torito boot record at all
        --no-joliet        do not add the Joliet (original-name) namespace
    -q, --quiet            suppress the summary output
    -h, --help             show this help
    -V, --version          show the version

EXAMPLES:
    fs2iso tools.iso D:\\fw\\efi_tools          # fs0: shows efi_tools/...
    fs2iso --flat fix.iso FixPkg/              # fs0: shows FixPkg contents directly
    fs2iso --boot-efi Shell.efi shell.iso Shell.efi mydir/

The image carries both an ISO9660 base tree (uppercase, always readable) and a
Joliet tree (original names incl. Chinese/long names). Mount it on the BMC as
CD/DVD virtual media; in the EFI shell run 'map -r' then 'fs0:'.

";

fn usage_err(msg: &str) -> ! {
    eprintln!("fs2iso: {}", msg);
    eprintln!("Try 'fs2iso --help' for usage.");
    std::process::exit(2);
}

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();

    let mut opts = Options::default();
    let mut quiet = false;
    let mut positional: Vec<OsString> = Vec::new();

    let mut i = 0;
    let mut no_more_flags = false;
    while i < args.len() {
        let a = &args[i];
        let s = a.to_str();
        if !no_more_flags && s.is_some() {
            let s = s.unwrap();
            match s {
                "--" => no_more_flags = true,
                "-h" | "--help" => {
                    print!("{}", HELP);
                    std::process::exit(0);
                }
                "-V" | "--version" => {
                    println!("fs2iso {}", env!("CARGO_PKG_VERSION"));
                    std::process::exit(0);
                }
                "-q" | "--quiet" => quiet = true,
                "--flat" => opts.flat = true,
                "--no-eltorito" => opts.no_eltorito = true,
                "--no-joliet" => opts.no_joliet = true,
                "-l" | "--label" | "--boot-efi" => {
                    let val = args
                        .get(i + 1)
                        .unwrap_or_else(|| usage_err(&format!("{} needs a value", s)));
                    if s == "-l" || s == "--label" {
                        opts.label = Some(val.to_string_lossy().into_owned());
                    } else {
                        opts.boot_efi = Some(PathBuf::from(val));
                    }
                    i += 1;
                }
                _ if s.starts_with("--label=") => {
                    opts.label = Some(s["--label=".len()..].to_string());
                }
                _ if s.starts_with("--boot-efi=") => {
                    opts.boot_efi = Some(PathBuf::from(&s["--boot-efi=".len()..]));
                }
                _ if s.starts_with('-') && s.len() > 1 => {
                    usage_err(&format!("unknown option '{}'", s));
                }
                _ => positional.push(a.clone()),
            }
        } else {
            positional.push(a.clone());
        }
        i += 1;
    }

    if positional.len() < 2 {
        usage_err("expected <OUTPUT.iso> and at least one <PATH>");
    }
    let output = PathBuf::from(&positional[0]);
    let inputs: Vec<PathBuf> = positional[1..].iter().map(PathBuf::from).collect();

    let is_iso = |p: &OsStr| {
        let s = p.to_string_lossy().to_lowercase();
        s.ends_with(".iso") || s.ends_with(".img")
    };
    if !is_iso(output.as_os_str()) {
        eprintln!(
            "fs2iso: warning: output '{}' does not end in .iso/.img",
            output.display()
        );
    }

    match build_iso(&output, &inputs, &opts) {
        Ok(sum) => {
            if !quiet {
                let mb = sum.payload_bytes as f64 / (1024.0 * 1024.0);
                let total_mb = sum.sectors as f64 * 2048.0 / (1024.0 * 1024.0);
                println!("wrote {} ({:.1} MiB)", output.display(), total_mb);
                println!(
                    "  {} dirs, {} files, {:.2} MiB payload",
                    sum.dirs, sum.files, mb
                );
                println!("  label: {}", sum.label);
                println!(
                    "  namespaces: ISO9660{}",
                    if sum.joliet { " + Joliet" } else { "" }
                );
                match &sum.boot_path {
                    Some(p) => println!("  El Torito EFI boot: /{}", p),
                    None => println!("  El Torito: none (data CD)"),
                }
            }
        }
        Err(e) => {
            eprintln!("fs2iso: error: {}", e);
            std::process::exit(1);
        }
    }
}
