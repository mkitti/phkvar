//! `phkvar create`: walks the inputs and writes a standalone or hybrid phkv store.

use crate::paths::key_of;
use crate::{CREATOR_META, Compression, Context, Result, TYPE_PROP};
use refkv_phkv::{Codec, HybridWriter, Options, Prop, Writer};
use refkv_vzip::pb::{Range, Source};
use std::collections::HashMap;
use std::fs::{self, File, Metadata};
use std::io::{BufWriter, Read};
use std::path::{Path, PathBuf};

pub struct Args {
    pub archive: PathBuf,
    pub paths: Vec<PathBuf>,
    pub directory: Option<PathBuf>,
    pub compression: Compression,
    pub zip: bool,
    pub dedup: bool,
    pub reference: bool,
    pub verbose: bool,
}

/// The two writers phkv offers, behind one interface.
enum Out {
    Standalone(Writer<BufWriter<File>>),
    Hybrid(HybridWriter<BufWriter<File>>),
}

impl Out {
    fn add_bytes(&mut self, key: &str, data: &[u8], c: Compression) -> std::result::Result<(), refkv_phkv::Error> {
        match self {
            Out::Standalone(w) => w.add_bytes_with(key, data, codec(c)),
            Out::Hybrid(w) => w.add_bytes(key, data, c == Compression::Deflate, false),
        }
    }

    fn add_file(&mut self, key: &str, path: &Path) -> std::result::Result<(), refkv_phkv::Error> {
        match self {
            Out::Standalone(w) => w.add_file(key, path),
            Out::Hybrid(w) => w.add_file(key, path),
        }
    }

    fn source(&mut self, s: Source) -> std::result::Result<u32, refkv_phkv::Error> {
        match self {
            Out::Standalone(w) => w.source(s),
            Out::Hybrid(w) => w.source(s),
        }
    }

    fn add_ranges(&mut self, key: &str, ranges: Vec<Range>) -> std::result::Result<(), refkv_phkv::Error> {
        match self {
            Out::Standalone(w) => w.add_ranges(key, &ranges),
            Out::Hybrid(w) => w.add_ranges(key, ranges),
        }
    }

    fn set_property(&mut self, key: &str, name: &str, v: Prop) -> std::result::Result<(), refkv_phkv::Error> {
        match self {
            Out::Standalone(w) => w.set_property(key, name, v),
            Out::Hybrid(w) => w.set_property(key, name, v),
        }
    }

    fn set_metadata(&mut self, name: &str, v: &[u8]) -> std::result::Result<(), refkv_phkv::Error> {
        match self {
            Out::Standalone(w) => w.set_metadata(name, v),
            Out::Hybrid(w) => w.set_metadata(name, v),
        }
    }

    fn close(self) -> std::result::Result<BufWriter<File>, refkv_phkv::Error> {
        match self {
            Out::Standalone(w) => w.close(),
            Out::Hybrid(w) => w.close(),
        }
    }
}

fn codec(c: Compression) -> Codec {
    match c {
        Compression::None => Codec::None,
        Compression::Deflate => Codec::Deflate,
        Compression::Zstd => Codec::Zstd,
    }
}

struct State<'a> {
    args: &'a Args,
    out: Out,
    /// the absolute path of the archive being written, so that it is not added to itself
    archive: Option<PathBuf>,
    /// (size, xxh3) of each file's content, and the first key and path that had it
    seen: HashMap<(u64, u64), Vec<(String, PathBuf)>>,
    count: u64,
}

pub fn create(args: &Args) -> Result<()> {
    if args.zip && args.compression == Compression::Zstd {
        return Err("--zip archives are ZIP files: use --compression deflate or none".into());
    }
    let file = File::create(&args.archive).ctx(args.archive.display())?;
    let archive = fs::canonicalize(&args.archive).ok();
    let w = BufWriter::with_capacity(1 << 20, file);
    let out = if args.zip {
        Out::Hybrid(HybridWriter::new(w, refkv_vzip::Options::default(), Options::default()).ctx("hybrid writer")?)
    } else {
        Out::Standalone(Writer::new(w, Options::default()))
    };
    if let Some(d) = &args.directory {
        std::env::set_current_dir(d).ctx(d.display())?;
    }
    let mut st = State { args, out, archive, seen: HashMap::new(), count: 0 };
    for p in &args.paths {
        let key = key_of(p)?;
        add_path(&mut st, p, &key)?;
    }
    st.out.set_metadata(CREATOR_META, format!("phkvar {}", env!("CARGO_PKG_VERSION")).as_bytes()).ctx("metadata")?;
    let count = st.count;
    st.out.close().ctx(args.archive.display())?;
    if args.verbose {
        eprintln!("{count} entries");
    }
    Ok(())
}

/// Adds `path` under `key` (empty for `.`), recursing into directories.
fn add_path(st: &mut State, path: &Path, key: &str) -> Result<()> {
    let md = fs::symlink_metadata(path).ctx(path.display())?;
    let ft = md.file_type();
    if ft.is_dir() {
        if !key.is_empty() {
            let k = format!("{key}/");
            put(st, &k, path, &md, |out| out.add_bytes(&k, b"", Compression::None))?;
        }
        let mut names: Vec<_> =
            fs::read_dir(path).ctx(path.display())?.map(|e| e.map(|e| e.file_name())).collect::<std::io::Result<_>>().ctx(path.display())?;
        names.sort();
        for name in names {
            let name_str = name.to_str().ok_or_else(|| format!("{}: not a UTF-8 file name", path.join(&name).display()))?;
            let child = if key.is_empty() { name_str.to_string() } else { format!("{key}/{name_str}") };
            add_path(st, &path.join(&name), &child)?;
        }
        Ok(())
    } else if ft.is_symlink() {
        let target = fs::read_link(path).ctx(path.display())?;
        let target = target.to_str().ok_or_else(|| format!("{}: link target is not UTF-8", path.display()))?.to_string();
        put(st, key, path, &md, |out| out.add_bytes(key, target.as_bytes(), Compression::None))?;
        st.out.set_property(key, TYPE_PROP, Prop::Str("symlink".into())).ctx(key)
    } else if ft.is_file() {
        if key.is_empty() {
            return Err(format!("{}: no name to archive it under", path.display()));
        }
        if st.archive.is_some() && fs::canonicalize(path).ok() == st.archive {
            eprintln!("phkvar: {}: skipping the archive itself", path.display());
            return Ok(());
        }
        add_file(st, path, key, &md)
    } else {
        eprintln!("phkvar: {}: skipping a special file", path.display());
        Ok(())
    }
}

fn add_file(st: &mut State, path: &Path, key: &str, md: &Metadata) -> Result<()> {
    let size = md.len();
    if st.args.reference && size > 0 {
        let (crc, _) = digest(path)?;
        let mut src = Source::url(file_url(path)?);
        src.size = Some(size);
        src.modified_not_after = mtime_ns(md).map(|ns| ns.div_euclid(1_000_000_000) + 1);
        let i = st.out.source(src).ctx(key)?;
        let range = Range { crc32c: Some(crc), ..Range::of(i, 0, size) };
        return put(st, key, path, md, |out| out.add_ranges(key, vec![range]));
    }
    if st.args.dedup && size > 0 {
        let (_, h) = digest(path)?;
        let earlier = st.seen.get(&(size, h)).into_iter().flatten().find(|(_, p)| same_content(p, path).unwrap_or(false)).map(|(k, _)| k.clone());
        if let Some(first) = earlier {
            let i = st.out.source(Source::key(first.as_str())).ctx(key)?;
            return put(st, key, path, md, |out| out.add_ranges(key, vec![Range::of(i, 0, size)]));
        }
        st.seen.entry((size, h)).or_default().push((key.to_string(), path.to_path_buf()));
    }
    let c = st.args.compression;
    put(st, key, path, md, |out| {
        if c == Compression::None {
            out.add_file(key, path)
        } else {
            let data = fs::read(path)?;
            // like zip, store a file whose compressed form is not smaller as it is
            let c = if compresses(&data, c) { c } else { Compression::None };
            out.add_bytes(key, &data, c)
        }
    })
}

/// Whether `data` comes out smaller under `c`. The writer encodes the value again, so this costs
/// a second compression of the files that do compress; the ones that do not are not encoded twice.
fn compresses(data: &[u8], c: Compression) -> bool {
    data.len() > 64 && refkv_phkv::format::encode(codec(c), data).len() < data.len()
}

/// Adds one entry with `f`, then its mode and mtime.
fn put(st: &mut State, key: &str, path: &Path, md: &Metadata, f: impl FnOnce(&mut Out) -> std::result::Result<(), refkv_phkv::Error>) -> Result<()> {
    f(&mut st.out).ctx(path.display())?;
    if let Some(mode) = mode(md) {
        st.out.set_property(key, "mode", Prop::U64(mode)).ctx(key)?;
    }
    if let Some(ns) = mtime_ns(md) {
        st.out.set_property(key, "mtime", Prop::I64(ns)).ctx(key)?;
    }
    st.count += 1;
    if st.args.verbose {
        println!("{key}");
    }
    Ok(())
}

#[cfg(unix)]
fn mode(md: &Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(u64::from(md.mode()) & 0o7777)
}

#[cfg(not(unix))]
fn mode(_: &Metadata) -> Option<u64> {
    None
}

/// The modification time in nanoseconds since the Unix epoch.
fn mtime_ns(md: &Metadata) -> Option<i64> {
    let t = md.modified().ok()?;
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).ok(),
        Err(e) => i64::try_from(e.duration().as_nanos()).ok().map(|n| -n),
    }
}

/// The CRC-32C and xxh3 of a file's content, streamed.
fn digest(path: &Path) -> Result<(u32, u64)> {
    let mut f = File::open(path).ctx(path.display())?;
    let mut buf = vec![0u8; 1 << 20];
    let (mut crc, mut h) = (0u32, xxhash_rust::xxh3::Xxh3::new());
    loop {
        let n = f.read(&mut buf).ctx(path.display())?;
        if n == 0 {
            return Ok((crc, h.digest()));
        }
        crc = crc32c::crc32c_append(crc, &buf[..n]);
        h.update(&buf[..n]);
    }
}

fn same_content(a: &Path, b: &Path) -> std::io::Result<bool> {
    let (mut fa, mut fb) = (File::open(a)?, File::open(b)?);
    let (mut ba, mut bb) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
    loop {
        let n = read_full(&mut fa, &mut ba)?;
        if n != read_full(&mut fb, &mut bb)? || ba[..n] != bb[..n] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
    }
}

fn read_full(f: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// The absolute `file://` URL of `path`, percent-encoding all but unreserved characters and `/`.
fn file_url(path: &Path) -> Result<String> {
    let abs = fs::canonicalize(path).ctx(path.display())?;
    let s = abs.to_str().ok_or_else(|| format!("{}: not a UTF-8 path", abs.display()))?;
    // drop the verbatim prefix Windows gives canonical paths
    let s = s.strip_prefix(r"\\?\").unwrap_or(s).replace('\\', "/");
    let mut url = String::from(if s.starts_with('/') { "file://" } else { "file:///" });
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            url.push(b as char);
        } else {
            url.push_str(&format!("%{b:02X}"));
        }
    }
    Ok(url)
}
