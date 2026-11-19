//! Mapping between filesystem paths and archive keys, and the checks that keep extraction inside
//! its target directory.

use crate::Result;
use std::path::{Component, Path, PathBuf};

/// The key of `path` as given on the command line: its normal components joined by `/`. Leading
/// `/`, `./` and drive prefixes are dropped (as tar does); `..` is refused.
pub fn key_of(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_str().ok_or_else(|| format!("{}: not a UTF-8 path", path.display()))?),
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
            Component::ParentDir => return Err(format!("{}: paths with `..` are not archived", path.display())),
        }
    }
    Ok(parts.join("/"))
}

/// The relative path a key extracts to, or an error if it could escape the target directory:
/// absolute keys, `..` or `.` components, empty components (other than a directory's trailing
/// `/`), and on Windows, backslashes, drive letters and reserved characters.
pub fn safe_relative(key: &str) -> Result<PathBuf> {
    let body = key.strip_suffix('/').unwrap_or(key);
    if body.is_empty() {
        return Err(format!("{key:?}: an empty path"));
    }
    let mut out = PathBuf::new();
    for part in body.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(format!("{key:?}: unsafe path component {part:?}"));
        }
        if part.contains('\0') || (cfg!(windows) && part.contains(['\\', ':', '<', '>', '"', '|', '?', '*'])) {
            return Err(format!("{key:?}: unsafe path component {part:?}"));
        }
        out.push(part);
    }
    Ok(out)
}

/// Fails if any existing ancestor of `rel` below `root` is a symbolic link, so that a symlink
/// extracted earlier cannot redirect a later member outside `root`.
pub fn check_no_symlink_ancestors(root: &Path, rel: &Path) -> Result<()> {
    let mut at = root.to_path_buf();
    let comps: Vec<_> = rel.components().collect();
    for c in &comps[..comps.len().saturating_sub(1)] {
        at.push(c);
        match std::fs::symlink_metadata(&at) {
            Ok(m) if m.file_type().is_symlink() => return Err(format!("{}: a parent directory is a symbolic link", rel.display())),
            Ok(_) => {}
            Err(_) => return Ok(()), // the rest does not exist yet
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(key_of(Path::new("./a/b")).unwrap(), "a/b");
        assert_eq!(key_of(Path::new("/abs/x")).unwrap(), "abs/x");
        assert_eq!(key_of(Path::new(".")).unwrap(), "");
        assert!(key_of(Path::new("a/../b")).is_err());
    }

    #[test]
    fn safety() {
        assert_eq!(safe_relative("a/b").unwrap(), Path::new("a").join("b"));
        assert_eq!(safe_relative("dir/").unwrap(), Path::new("dir"));
        for bad in ["/etc/passwd", "../x", "a/../../x", "a//b", "./a", "", "/"] {
            assert!(safe_relative(bad).is_err(), "{bad}");
        }
    }
}
