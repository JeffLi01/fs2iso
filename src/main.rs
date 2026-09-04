//! fs2iso CLI — pack files/directories into an ISO9660 image for BMC virtual
//! media / UEFI shell (fs0) use.

use clap::Parser;
use fs2iso::{build_iso, Options};
use std::path::PathBuf;

/// Pack files/directories into an ISO 9660 image for BMC virtual media /
/// UEFI shell (fs0) use.
///
/// Each PATH is packed under its own name at the ISO root:
///
///   - file -> a file at the root
///   - dir  -> a directory (whole subtree) at the root
///
/// With --flat, a directory's *contents* are merged into the root instead.
///
/// The image carries both an ISO9660 base tree (uppercase names, readable by
/// any UEFI firmware) and a Joliet tree (original names incl. Chinese / long
/// names). Mount it on the BMC as CD/DVD virtual media, then in the EFI shell
/// run `map -r` and `fs0:`.
#[derive(Parser)]
#[command(
    name = "fs2iso",
    version,
    about = "Pack files/directories into an ISO 9660 image (BMC virtual media / UEFI shell)",
    after_help = "EXAMPLES:\n    \
        fs2iso tools.iso D:\\fw\\efi_tools        # fs0: shows efi_tools/...\n    \
        fs2iso --flat fix.iso FixPkg/             # fs0: shows FixPkg contents directly\n    \
        fs2iso --boot-efi Shell.efi shell.iso Shell.efi mydir/\n"
)]
struct Cli {
    /// Volume label (default: derived from the output file name)
    #[arg(short = 'l', long, value_name = "NAME")]
    label: Option<String>,

    /// Pack directory contents into the ISO root (mkisofs style)
    #[arg(long)]
    flat: bool,

    /// Use payload FILE as the El Torito EFI boot image
    /// (auto-detected: EFI/BOOT/BOOTX64.EFI unless --no-eltorito)
    #[arg(long, value_name = "FILE")]
    boot_efi: Option<PathBuf>,

    /// Do not add an El Torito boot record at all
    #[arg(long)]
    no_eltorito: bool,

    /// Do not add the Joliet (original-name) namespace
    #[arg(long)]
    no_joliet: bool,

    /// Suppress the summary output
    #[arg(short, long)]
    quiet: bool,

    /// Output image path (.iso or .img)
    output: PathBuf,

    /// Files or directories to pack into the image
    #[arg(required = true, value_name = "PATH")]
    paths: Vec<PathBuf>,
}

fn main() {
    let cli = Cli::parse();

    let opts = Options {
        label: cli.label,
        flat: cli.flat,
        boot_efi: cli.boot_efi,
        no_eltorito: cli.no_eltorito,
        no_joliet: cli.no_joliet,
    };

    let ext = cli
        .output
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if ext != "iso" && ext != "img" {
        eprintln!(
            "fs2iso: warning: output '{}' does not end in .iso/.img",
            cli.output.display()
        );
    }

    match build_iso(&cli.output, &cli.paths, &opts) {
        Ok(sum) => {
            if !cli.quiet {
                let mb = sum.payload_bytes as f64 / (1024.0 * 1024.0);
                let total_mb = sum.sectors as f64 * 2048.0 / (1024.0 * 1024.0);
                println!("wrote {} ({:.1} MiB)", cli.output.display(), total_mb);
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
                    Some(p) => {
                        // first path segment is the synthetic in-memory root;
                        // the real ISO path starts at the packed payload name
                        let rel = match p.split_once('/') {
                            Some((_, rest)) => rest,
                            None => p.as_str(),
                        };
                        println!("  El Torito EFI boot: /{}", rel);
                    }
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
