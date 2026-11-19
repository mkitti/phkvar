//! phkvar: a tar/zip-like archiver whose archives are phkv stores (refkv's perfect-hash binding,
//! https://github.com/d-v-b/refkv/blob/main/spec/bindings/phkv.md).
//!
//! Each archived path is a key; a file's bytes are its value; its mode and mtime are the
//! registered phkv properties `mode` and `mtime`. Because a phkv store finds any key through a
//! perfect hash, reading one member of a large archive costs a few small reads, not a scan.

mod create;
mod paths;
mod read;

use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use std::process::ExitCode;

/// The property that marks entries that are not regular files or directories.
pub const TYPE_PROP: &str = "github.com/mkitti/phkvar:type";
/// The METADATA entry naming the program that wrote the archive.
pub const CREATOR_META: &str = "github.com/mkitti/phkvar:creator";

#[derive(Parser)]
#[command(name = "phkvar", version, about = "Archive files into phkv stores, list them, and extract them")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Compression {
    None,
    Deflate,
    Zstd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create an archive from files and directories (like `tar c`).
    #[command(visible_alias = "c")]
    Create {
        /// The archive to write.
        archive: PathBuf,
        /// Files and directories to add, recursively.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Change to this directory before adding the paths.
        #[arg(short = 'C', long)]
        directory: Option<PathBuf>,
        /// How each file's bytes are compressed.
        #[arg(short = 'c', long, value_enum, default_value_t = Compression::None)]
        compression: Compression,
        /// Shorthand for `--compression zstd`.
        #[arg(short = 'z', long, conflicts_with = "compression")]
        zstd: bool,
        /// Write a hybrid store: a ZIP (vzip) archive that also carries the phkv index, so that
        /// ordinary ZIP tools can open it too. Only `none` and `deflate` compression apply.
        #[arg(long)]
        zip: bool,
        /// Store a file whose content equals an earlier file's as a reference to that file's bytes.
        #[arg(long)]
        dedup: bool,
        /// Do not copy the files: record each one as a reference to its absolute file:// URL, pinned
        /// to its size and modification time, with a CRC-32C (an index, not a copy).
        #[arg(long = "ref", conflicts_with = "dedup")]
        reference: bool,
        /// List the keys as they are added.
        #[arg(short, long)]
        verbose: bool,
    },
    /// List the members of an archive (like `tar t`).
    #[command(visible_alias = "t")]
    List {
        archive: PathBuf,
        /// Only list keys that start with this prefix.
        prefix: Option<String>,
        /// Show the type, mode, size, stored size, codec and mtime of each member.
        #[arg(short, long)]
        verbose: bool,
    },
    /// Extract members of an archive (like `tar x`).
    #[command(visible_alias = "x")]
    Extract {
        archive: PathBuf,
        /// Members to extract: a key, or a directory key (with or without its trailing `/`) and
        /// everything under it. All members if none are given.
        members: Vec<String>,
        /// Extract into this directory.
        #[arg(short = 'C', long, default_value = ".")]
        directory: PathBuf,
        /// Resolve references to file:// URLs (as `create --ref` writes) by reading local files.
        #[arg(long)]
        resolve_files: bool,
        #[arg(short, long)]
        verbose: bool,
    },
    /// Write one member's bytes, or a byte range of them, to standard output.
    Cat {
        archive: PathBuf,
        key: String,
        /// The bytes START:END (END exclusive; either may be omitted, a negative START counts
        /// from the end).
        #[arg(short, long, allow_hyphen_values = true)]
        range: Option<String>,
        #[arg(long)]
        resolve_files: bool,
    },
    /// Read every member and check its CRC-32C (like `unzip -t`).
    Test {
        archive: PathBuf,
        #[arg(long)]
        resolve_files: bool,
        #[arg(short, long)]
        verbose: bool,
    },
    /// Describe the archive itself: profile, keys, sources and metadata.
    Info { archive: PathBuf },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let r = match cli.cmd {
        Cmd::Create { archive, paths, directory, compression, zstd, zip, dedup, reference, verbose } => {
            let compression = if zstd { Compression::Zstd } else { compression };
            create::create(&create::Args { archive, paths, directory, compression, zip, dedup, reference, verbose })
        }
        Cmd::List { archive, prefix, verbose } => read::list(&archive, prefix.as_deref().unwrap_or(""), verbose),
        Cmd::Extract { archive, members, directory, resolve_files, verbose } => read::extract(&archive, &members, &directory, resolve_files, verbose),
        Cmd::Cat { archive, key, range, resolve_files } => read::cat(&archive, &key, range.as_deref(), resolve_files),
        Cmd::Test { archive, resolve_files, verbose } => read::test(&archive, resolve_files, verbose),
        Cmd::Info { archive } => read::info(&archive),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("phkvar: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Errors are reported as text: this is a command-line tool.
pub type Result<T> = std::result::Result<T, String>;

pub trait Context<T> {
    fn ctx(self, what: impl std::fmt::Display) -> Result<T>;
}

impl<T, E: std::fmt::Display> Context<T> for std::result::Result<T, E> {
    fn ctx(self, what: impl std::fmt::Display) -> Result<T> {
        self.map_err(|e| format!("{what}: {e}"))
    }
}
