//! End-to-end tests of the `phkvar` binary.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn phkvar(dir: &Path, args: &[&str]) -> Output {
    let out = Command::new(env!("CARGO_BIN_EXE_phkvar")).current_dir(dir).args(args).output().unwrap();
    assert!(out.status.success(), "phkvar {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    out
}

fn tree(root: &Path) {
    fs::create_dir_all(root.join("d/sub/empty")).unwrap();
    fs::write(root.join("d/a.txt"), "alpha\n").unwrap();
    fs::write(root.join("d/sub/a-again.txt"), "alpha\n").unwrap();
    fs::write(root.join("d/sub/nums.txt"), (0..20_000).map(|i| format!("{i}\n")).collect::<String>()).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("../a.txt", root.join("d/sub/link")).unwrap();
}

fn same_tree(a: &Path, b: &Path) {
    let mut names: Vec<_> = fs::read_dir(a).unwrap().map(|e| e.unwrap().file_name()).collect();
    names.sort();
    let mut other: Vec<_> = fs::read_dir(b).unwrap().map(|e| e.unwrap().file_name()).collect();
    other.sort();
    assert_eq!(names, other, "{}", a.display());
    for n in names {
        let (x, y) = (a.join(&n), b.join(&n));
        let (mx, my) = (fs::symlink_metadata(&x).unwrap(), fs::symlink_metadata(&y).unwrap());
        assert_eq!(mx.file_type(), my.file_type(), "{}", x.display());
        if mx.file_type().is_symlink() {
            assert_eq!(fs::read_link(&x).unwrap(), fs::read_link(&y).unwrap());
        } else if mx.is_dir() {
            same_tree(&x, &y);
        } else {
            assert_eq!(fs::read(&x).unwrap(), fs::read(&y).unwrap(), "{}", x.display());
            assert_eq!(mx.modified().unwrap(), my.modified().unwrap(), "{}", x.display());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(mx.permissions().mode(), my.permissions().mode(), "{}", x.display());
            }
        }
    }
}

fn round_trip(create: &[&str]) {
    let t = tempfile::tempdir().unwrap();
    tree(t.path());
    let mut args = vec!["create"];
    args.extend_from_slice(create);
    args.extend_from_slice(&["out.phkv", "d"]);
    phkvar(t.path(), &args);
    phkvar(t.path(), &["test", "out.phkv"]);
    phkvar(t.path(), &["extract", "out.phkv", "-C", "x"]);
    same_tree(&t.path().join("d"), &t.path().join("x/d"));
}

#[test]
fn round_trips() {
    round_trip(&[]);
    round_trip(&["-z"]);
    round_trip(&["--compression", "deflate", "--dedup"]);
    round_trip(&["--zip"]);
    round_trip(&["--zip", "-c", "deflate", "--dedup"]);
}

#[test]
fn list_and_cat() {
    let t = tempfile::tempdir().unwrap();
    tree(t.path());
    phkvar(t.path(), &["c", "-z", "a.phkv", "d"]);
    let keys = String::from_utf8(phkvar(t.path(), &["t", "a.phkv"]).stdout).unwrap();
    assert!(keys.lines().any(|k| k == "d/sub/empty/"));
    assert!(keys.lines().any(|k| k == "d/a.txt"));
    let only = String::from_utf8(phkvar(t.path(), &["t", "a.phkv", "d/sub/"]).stdout).unwrap();
    assert!(only.lines().all(|k| k.starts_with("d/sub/")));
    assert_eq!(phkvar(t.path(), &["cat", "a.phkv", "d/a.txt"]).stdout, b"alpha\n");
    assert_eq!(phkvar(t.path(), &["cat", "a.phkv", "d/sub/nums.txt", "-r", "2:6"]).stdout, b"1\n2\n");
    assert_eq!(phkvar(t.path(), &["cat", "a.phkv", "d/sub/nums.txt", "-r", "-6:"]).stdout, b"19999\n");
}

#[test]
fn dedup_stores_once() {
    let t = tempfile::tempdir().unwrap();
    fs::create_dir(t.path().join("d")).unwrap();
    let big: Vec<u8> = (0..1_000_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
    fs::write(t.path().join("d/one"), &big).unwrap();
    fs::write(t.path().join("d/two"), &big).unwrap();
    phkvar(t.path(), &["c", "plain.phkv", "d"]);
    phkvar(t.path(), &["c", "--dedup", "dedup.phkv", "d"]);
    let (plain, dedup) = (fs::metadata(t.path().join("plain.phkv")).unwrap().len(), fs::metadata(t.path().join("dedup.phkv")).unwrap().len());
    assert!(plain > 2_000_000 && dedup < 1_100_000, "{plain} {dedup}");
    assert_eq!(phkvar(t.path(), &["cat", "dedup.phkv", "d/two"]).stdout, big);
}

#[test]
fn references_need_resolving_and_pins_hold() {
    let t = tempfile::tempdir().unwrap();
    tree(t.path());
    phkvar(t.path(), &["c", "--ref", "r.phkv", "d"]);
    let out = Command::new(env!("CARGO_BIN_EXE_phkvar")).current_dir(t.path()).args(["cat", "r.phkv", "d/a.txt"]).output().unwrap();
    assert!(!out.status.success());
    assert_eq!(phkvar(t.path(), &["cat", "--resolve-files", "r.phkv", "d/a.txt"]).stdout, b"alpha\n");
    fs::write(t.path().join("d/a.txt"), "changed!\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_phkvar")).current_dir(t.path()).args(["cat", "--resolve-files", "r.phkv", "d/a.txt"]).output().unwrap();
    assert!(!out.status.success(), "a changed file must not resolve");
}

#[test]
fn extract_selected_members() {
    let t = tempfile::tempdir().unwrap();
    tree(t.path());
    phkvar(t.path(), &["c", "a.phkv", "d"]);
    phkvar(t.path(), &["x", "a.phkv", "-C", "x", "d/sub"]);
    assert!(t.path().join("x/d/sub/nums.txt").exists());
    assert!(t.path().join("x/d/sub/empty").is_dir());
    assert!(!t.path().join("x/d/a.txt").exists());
}
