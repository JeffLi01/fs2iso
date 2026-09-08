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
/// All payload files are packed into a FAT image (esp.img) at their original
/// paths — no special-casing, no injected boot file — and the El Torito
/// entry points at esp.img so firmware exposes the FAT volume to the EFI
/// shell (fs0/fsX). The payload is also kept in the ISO9660(+Joliet) data
/// tree for readers that mount the disc data directly (Windows).
#[derive(Parser)]
#[command(
    name = "fs2iso",
    version,
    about = "Pack files/directories into an ISO 9660 image (BMC virtual media / UEFI shell)",
    after_help = "EXAMPLES:\n    fs2iso tools.iso D:\\fw\\efi_tools    # files also inside esp.img (FAT)\n    fs2iso --flat fix.iso FixPkg/         # contents at root, mirrored into esp.img\n    fs2iso --no-eltorito data.iso files/  # plain ISO9660 data disc, no esp.img\n"
)]
struct Cli {
    /// Volume label (default: derived from the output file name)
    #[arg(short = 'l', long, value_name = "NAME")]
    label: Option<String>,

    /// Pack directory contents into the ISO root (mkisofs style)
    #[arg(long)]
    flat: bool,

    /// Do not add an El Torito boot record at all
    #[arg(long)]
    no_eltorito: bool,

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
        no_eltorito: cli.no_eltorito,
    };

    let extension = cli
        .output
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if extension != "iso" && extension != "img" {
        eprintln!(
            "fs2iso: warning: output '{}' does not end in .iso/.img",
            cli.output.display()
        );
    }

    match build_iso(&cli.output, &cli.paths, &opts) {
        Ok(summary) => {
            if !cli.quiet {
                let payload_size_mib = summary.payload_bytes as f64 / (1024.0 * 1024.0);
                let image_size_mib = summary.sectors as f64 * 2048.0 / (1024.0 * 1024.0);
                println!("wrote {} ({:.1} MiB)", cli.output.display(), image_size_mib);
                println!(
                    "  {} dirs, {} files, {:.2} MiB payload",
                    summary.dirs, summary.files, payload_size_mib
                );
                println!("  label: {}", summary.label);
                println!("  namespaces: ISO9660 + Joliet");
                if summary.bootable {
                    println!(
                        "  FAT container esp.img: all payload files inside; El Torito -> esp.img"
                    );
                } else {
                    println!("  no esp.img (data disc, --no-eltorito)");
                }
            }
        }
        Err(e) => {
            eprintln!("fs2iso: error: {}", e);
            std::process::exit(1);
        }
    }
}
