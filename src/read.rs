//! The commands that read an archive: list, extract, cat, test and info.

use crate::paths::{check_no_symlink_ancestors, safe_relative};
use crate::{CREATOR_META, Context, Result, TYPE_PROP};
use refkv_phkv::{Codec, EntryInfo, NoUrls, Prop, Request, Resolver, Store};
use refkv_vzip::pb::{Source, SourceKind};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

/// Values without compression are read in windows of this many bytes, so that extracting a large
/// file does not hold it in memory.
const WINDOW: u64 = 16 << 20;

fn open(archive: &Path) -> Result<Store<File>> {
    let f = File::open(archive).ctx(archive.display())?;
    Store::open(f).ctx(archive.display())
}

/// Reads `file://` URL sources from the local filesystem, checking their size and modification-time
/// pins (spec/refkv.md §5.1). Only used when asked for: an archive could otherwise make extraction
/// copy any readable local file.
struct LocalFiles;

impl Resolver for LocalFiles {
    fn read(&self, source: &Source, offset: u64, length: u64) -> std::result::Result<Vec<u8>, String> {
        let url = source.url_str().ok_or("not a url source")?;
        let path = file_path(url)?;
        let f = File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let md = f.metadata().map_err(|e| e.to_string())?;
        if let Some(n) = source.size
            && md.len() != n
        {
            return Err(format!("{}: {} bytes, but the archive pinned {n}", path.display(), md.len()));
        }
        if let Some(t) = source.modified_not_after {
            let m = md.modified().map_err(|e| e.to_string())?.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
            if m > t {
                return Err(format!("{}: modified after the archive was written", path.display()));
            }
        }
        let mut buf = vec![0u8; length as usize];
        refkv_phkv::ReadAt::read_at(&f, offset, &mut buf).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(buf)
    }
}

/// The local path of a `file://` URL, percent-decoded.
fn file_path(url: &str) -> std::result::Result<PathBuf, String> {
    let rest = url.strip_prefix("file://").ok_or_else(|| format!("{url:?}: only file:// URLs are resolved"))?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return Err(format!("{url:?}: not a local file URL"));
    }
    let b = rest.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let h = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()).ok_or_else(|| format!("{url:?}: bad escape"))?;
            out.push(h);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    let s = String::from_utf8(out).map_err(|_| format!("{url:?}: not UTF-8"))?;
    // file:///C:/x on Windows
    let s = if cfg!(windows) && s.len() > 2 && s.as_bytes()[2] == b':' { s[1..].to_string() } else { s };
    Ok(PathBuf::from(s))
}

fn resolver(resolve_files: bool) -> &'static dyn Resolver {
    if resolve_files { &LocalFiles } else { &NoUrls }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Type {
    File,
    Dir,
    Symlink,
}

fn entry_type(store: &Store<File>, key: &str) -> Result<Type> {
    if key.ends_with('/') {
        return Ok(Type::Dir);
    }
    Ok(match store.property(key, TYPE_PROP).ctx(key)?.flatten() {
        Some(Prop::Str(s)) if s == "symlink" => Type::Symlink,
        _ => Type::File,
    })
}

fn prop_u64(store: &Store<File>, key: &str, name: &str) -> Result<Option<u64>> {
    Ok(match store.property(key, name).ctx(key)?.flatten() {
        Some(Prop::U64(v)) => Some(v),
        _ => None,
    })
}

fn prop_i64(store: &Store<File>, key: &str, name: &str) -> Result<Option<i64>> {
    Ok(match store.property(key, name).ctx(key)?.flatten() {
        Some(Prop::I64(v)) => Some(v),
        _ => None,
    })
}

/// Streams the value of `key` to `out`, and gives its size. Uncompressed values are read in
/// windows and checked against their CRC-32C here; the others are read whole, which the store
/// checks itself.
fn copy_value(store: &Store<File>, key: &str, res: &dyn Resolver, out: &mut dyn Write) -> Result<u64> {
    let info = store.entry_info(key).ctx(key)?.ok_or_else(|| format!("{key:?}: not in the archive"))?;
    if let EntryInfo::Bytes { codec: Codec::None, size, crc32c, .. } = info
        && size > WINDOW
    {
        let (mut at, mut crc) = (0u64, 0u32);
        while at < size {
            let end = (at + WINDOW).min(size);
            let b = store.get(key, Request::Range(at, end), res).ctx(key)?.ok_or_else(|| format!("{key:?}: vanished"))?;
            crc = crc32c::crc32c_append(crc, &b);
            out.write_all(&b).ctx(key)?;
            at = end;
        }
        if crc32c.is_some_and(|c| c != crc) {
            return Err(format!("{key:?}: the value does not match its CRC-32C"));
        }
        return Ok(size);
    }
    let b = store.get(key, Request::Whole, res).ctx(key)?.ok_or_else(|| format!("{key:?}: not in the archive"))?;
    out.write_all(&b).ctx(key)?;
    Ok(b.len() as u64)
}

pub fn list(archive: &Path, prefix: &str, verbose: bool) -> Result<()> {
    let store = open(archive)?;
    let keys = store.list(prefix).ctx(archive.display())?;
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    for key in keys {
        if !verbose {
            writeln!(out, "{key}").ctx("stdout")?;
            continue;
        }
        let t = entry_type(&store, &key)?;
        let mode = prop_u64(&store, &key, "mode")?;
        let mtime = prop_i64(&store, &key, "mtime")?;
        let (size, stored, how) = match store.entry_info(&key).ctx(&key)? {
            Some(EntryInfo::Bytes { codec, stored_length, size, .. }) => (size.to_string(), stored_length.to_string(), codec.name().to_string()),
            Some(EntryInfo::Reference { payload_length, .. }) => ("-".into(), payload_length.to_string(), "ref".into()),
            None => continue,
        };
        let kind = match t {
            Type::Dir => 'd',
            Type::Symlink => 'l',
            Type::File => '-',
        };
        let link = if t == Type::Symlink {
            format!(" -> {}", String::from_utf8_lossy(&store.get(&key, Request::Whole, &NoUrls).ctx(&key)?.unwrap_or_default()))
        } else {
            String::new()
        };
        writeln!(out, "{kind}{} {size:>12} {stored:>12} {how:<7} {} {key}{link}", mode_string(mode), mtime.map(format_time).unwrap_or_else(|| "-".repeat(19)))
            .ctx("stdout")?;
    }
    out.flush().ctx("stdout")
}

/// Whether `key` is selected by the members given on the command line.
fn selected(key: &str, members: &[String]) -> bool {
    members.is_empty()
        || members.iter().any(|m| {
            let m = m.trim_end_matches('/');
            key == m || key.strip_prefix(m).is_some_and(|rest| rest.starts_with('/'))
        })
}

pub fn extract(archive: &Path, members: &[String], root: &Path, resolve_files: bool, verbose: bool) -> Result<()> {
    let store = open(archive)?;
    let res = resolver(resolve_files);
    fs::create_dir_all(root).ctx(root.display())?;
    let keys: Vec<String> = store.list("").ctx(archive.display())?.into_iter().filter(|k| selected(k, members)).collect();
    if keys.is_empty() && !members.is_empty() {
        return Err(format!("no member matches {}", members.join(", ")));
    }
    let mut dirs = Vec::new();
    let mut failures = 0;
    for key in &keys {
        match extract_one(&store, key, root, res, &mut dirs) {
            Ok(()) => {
                if verbose {
                    println!("{key}");
                }
            }
            Err(e) => {
                eprintln!("phar: {e}");
                failures += 1;
            }
        }
    }
    // directories last, deepest first, so that adding their children does not change their mtime
    // and a read-only mode does not stop their children being written
    for (path, mode, mtime) in dirs.into_iter().rev() {
        set_mode(&path, mode);
        if let Some(t) = mtime
            && let Ok(f) = File::open(&path)
        {
            let _ = f.set_modified(system_time(t));
        }
    }
    if failures > 0 {
        return Err(format!("{failures} members could not be extracted"));
    }
    Ok(())
}

type Dirs = Vec<(PathBuf, Option<u64>, Option<i64>)>;

fn extract_one(store: &Store<File>, key: &str, root: &Path, res: &dyn Resolver, dirs: &mut Dirs) -> Result<()> {
    let rel = safe_relative(key)?;
    check_no_symlink_ancestors(root, &rel)?;
    let path = root.join(&rel);
    let mode = prop_u64(store, key, "mode")?;
    let mtime = prop_i64(store, key, "mtime")?;
    let t = entry_type(store, key)?;
    if t == Type::Dir {
        if fs::symlink_metadata(&path).is_ok_and(|m| !m.is_dir()) {
            fs::remove_file(&path).ctx(path.display())?;
        }
        fs::create_dir_all(&path).ctx(path.display())?;
        dirs.push((path, mode, mtime));
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ctx(parent.display())?;
    }
    // never write through whatever is already there (it could be a symlink)
    match fs::symlink_metadata(&path) {
        Ok(m) if m.is_dir() => return Err(format!("{}: a directory is in the way", path.display())),
        Ok(_) => fs::remove_file(&path).ctx(path.display())?,
        Err(_) => {}
    }
    if t == Type::Symlink {
        let target = store.get(key, Request::Whole, res).ctx(key)?.unwrap_or_default();
        let target = String::from_utf8(target).map_err(|_| format!("{key:?}: link target is not UTF-8"))?;
        return symlink(&target, &path);
    }
    let f = OpenOptions::new().write(true).create_new(true).open(&path).ctx(path.display())?;
    let mut w = io::BufWriter::with_capacity(1 << 20, f);
    copy_value(store, key, res, &mut w)?;
    let f = w.into_inner().map_err(|e| format!("{}: {}", path.display(), e.error()))?;
    if let Some(t) = mtime {
        f.set_modified(system_time(t)).ctx(path.display())?;
    }
    drop(f);
    set_mode(&path, mode);
    Ok(())
}

#[cfg(unix)]
fn symlink(target: &str, path: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, path).ctx(path.display())
}

#[cfg(not(unix))]
fn symlink(target: &str, path: &Path) -> Result<()> {
    Err(format!("{}: symbolic links (to {target:?}) are not extracted on this platform", path.display()))
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: Option<u64>) {
    use std::os::unix::fs::PermissionsExt;
    if let Some(m) = mode {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode((m & 0o7777) as u32));
    }
}

#[cfg(not(unix))]
fn set_mode(_: &Path, _: Option<u64>) {}

fn system_time(ns: i64) -> std::time::SystemTime {
    if ns >= 0 { UNIX_EPOCH + Duration::from_nanos(ns as u64) } else { UNIX_EPOCH - Duration::from_nanos(ns.unsigned_abs()) }
}

pub fn cat(archive: &Path, key: &str, range: Option<&str>, resolve_files: bool) -> Result<()> {
    let store = open(archive)?;
    let res = resolver(resolve_files);
    let stdout = io::stdout();
    let mut out = stdout.lock();
    match range {
        None => {
            copy_value(&store, key, res, &mut out)?;
        }
        Some(r) => {
            let req = parse_range(r)?;
            let b = store.get(key, req, res).ctx(key)?.ok_or_else(|| format!("{key:?}: not in the archive"))?;
            out.write_all(&b).ctx("stdout")?;
        }
    }
    out.flush().ctx("stdout")
}

/// `START:END`, `START:`, `:END` or `-N:` (the last N bytes).
fn parse_range(r: &str) -> Result<Request> {
    let (a, b) = r.split_once(':').ok_or_else(|| format!("{r:?}: a range is START:END"))?;
    let num = |s: &str| s.parse::<u64>().map_err(|_| format!("{r:?}: {s:?} is not a byte offset"));
    Ok(match (a, b) {
        ("", "") => Request::Whole,
        (a, "") if a.starts_with('-') => Request::Suffix(num(&a[1..])?),
        (a, "") => Request::Offset(num(a)?),
        ("", b) => Request::Range(0, num(b)?),
        (a, b) => Request::Range(num(a)?, num(b)?),
    })
}

pub fn test(archive: &Path, resolve_files: bool, verbose: bool) -> Result<()> {
    let store = open(archive)?;
    let res = resolver(resolve_files);
    let keys = store.list("").ctx(archive.display())?;
    let mut bad = 0;
    for key in &keys {
        match copy_value(&store, key, res, &mut io::sink()) {
            Ok(_) if verbose => println!("OK     {key}"),
            Ok(_) => {}
            Err(e) => {
                println!("FAILED {e}");
                bad += 1;
            }
        }
    }
    if bad > 0 {
        return Err(format!("{bad} of {} members failed", keys.len()));
    }
    println!("{}: {} members OK", archive.display(), keys.len());
    Ok(())
}

pub fn info(archive: &Path) -> Result<()> {
    let store = open(archive)?;
    let size = fs::metadata(archive).ctx(archive.display())?.len();
    println!("archive:  {}", archive.display());
    println!("profile:  {}", if store.superblock().hybrid() { "hybrid (a vzip/ZIP archive with a phkv index)" } else { "standalone phkv" });
    println!("size:     {size} bytes");
    println!("keys:     {}", store.list("").ctx(archive.display())?.len());
    println!("records:  {} (hidden ones included)", store.key_count());
    let sources = store.sources();
    println!("sources:  {}", sources.len());
    for (i, s) in sources.iter().enumerate() {
        let what = match &s.kind {
            SourceKind::Url(u) => format!("url {u}"),
            SourceKind::Key(k) => format!("key {k}"),
            SourceKind::Data(d) => format!("data ({} bytes)", d.len()),
        };
        println!("  [{i}] {what}");
    }
    for (name, value) in store.metadata() {
        let v = if name == CREATOR_META { String::from_utf8_lossy(value).into_owned() } else { format!("{} bytes", value.len()) };
        println!("metadata: {name} = {v}");
    }
    Ok(())
}

fn mode_string(mode: Option<u64>) -> String {
    let Some(m) = mode else { return "?????????".into() };
    let mut s = String::with_capacity(9);
    for (i, c) in "rwxrwxrwx".chars().enumerate() {
        s.push(if m & (1 << (8 - i)) != 0 { c } else { '-' });
    }
    s
}

/// `YYYY-MM-DD HH:MM:SS` (UTC) of nanoseconds since the epoch.
fn format_time(ns: i64) -> String {
    let secs = ns.div_euclid(1_000_000_000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(format_time(0), "1970-01-01 00:00:00");
        assert_eq!(format_time(1_700_000_000_000_000_000), "2023-11-14 22:13:20");
        assert_eq!(format_time(-1_000_000_000), "1969-12-31 23:59:59");
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range("2:5").unwrap(), Request::Range(2, 5));
        assert_eq!(parse_range("7:").unwrap(), Request::Offset(7));
        assert_eq!(parse_range(":5").unwrap(), Request::Range(0, 5));
        assert_eq!(parse_range("-3:").unwrap(), Request::Suffix(3));
        assert!(parse_range("x").is_err());
    }

    #[test]
    fn urls() {
        assert_eq!(file_path("file:///tmp/a%20b").unwrap(), PathBuf::from("/tmp/a b"));
        assert!(file_path("https://example.com/x").is_err());
    }

    #[test]
    fn members() {
        let m = vec!["a/b".to_string()];
        assert!(selected("a/b", &m));
        assert!(selected("a/b/", &m));
        assert!(selected("a/b/c", &m));
        assert!(!selected("a/bc", &m));
    }
}
