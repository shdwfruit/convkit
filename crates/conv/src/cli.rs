use std::path::PathBuf;

use clap::{Parser, Subcommand};
use convkit_core::{Backend, Resolver};

#[derive(Parser, Debug)]
#[command(
    name = "conv",
    version,
    about = "One command for everyday file conversion, offline"
)]
#[command(args_conflicts_with_subcommands = true)]
pub struct Cli {
    /// Input paths, then optionally an output path or a bare `.ext`.
    pub paths: Vec<PathBuf>,

    /// Target format for batch conversion, e.g. `--to jpg`.
    #[arg(long)]
    pub to: Option<String>,

    /// Print the backend command instead of running it.
    #[arg(long, global = true)]
    pub dry_run: bool,

    /// Emit machine-readable JSON.
    #[arg(long, global = true)]
    pub json: bool,

    /// Overwrite existing outputs.
    #[arg(short = 'y', long, global = true)]
    pub overwrite: bool,

    /// Suppress progress output.
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Write outputs into this directory.
    #[arg(short = 'o', long, global = true)]
    pub outdir: Option<PathBuf>,

    /// Parallel jobs in batch mode. Defaults to the core count.
    #[arg(short = 'j', long, global = true)]
    pub jobs: Option<usize>,

    #[arg(long, global = true, value_name = "PATH")]
    pub ffmpeg_path: Option<PathBuf>,
    #[arg(long, global = true, value_name = "PATH")]
    pub magick_path: Option<PathBuf>,
    #[arg(long, global = true, value_name = "PATH")]
    pub pandoc_path: Option<PathBuf>,
    #[arg(long, global = true, value_name = "PATH")]
    pub soffice_path: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Report which backends are installed and how to install the rest.
    Doctor,
    /// Download and verify a managed backend.
    Install { backend: String },
    /// List every supported conversion.
    Capabilities,
}

impl Cli {
    pub fn resolver(&self) -> Resolver {
        let mut r = Resolver::new();
        for (path, backend) in [
            (&self.ffmpeg_path, Backend::Ffmpeg),
            (&self.magick_path, Backend::Magick),
            (&self.pandoc_path, Backend::Pandoc),
            (&self.soffice_path, Backend::Soffice),
        ] {
            if let Some(p) = path {
                r.with_override(backend, p.clone());
            }
        }
        // ffprobe ships beside ffmpeg; honour the same override directory.
        if let Some(p) = &self.ffmpeg_path {
            if let Some(dir) = p.parent() {
                let probe = dir.join(if cfg!(windows) {
                    "ffprobe.exe"
                } else {
                    "ffprobe"
                });
                r.with_override(Backend::Ffprobe, probe);
            }
        }
        r
    }
}
