# phkvar

A `tar`/`zip`-like archiver whose archives are **phkv stores**: the perfect-hash
binding of [refkv](https://github.com/d-v-b/refkv)
([spec/bindings/phkv.md](https://github.com/d-v-b/refkv/blob/main/spec/bindings/phkv.md)).

> [!WARNING]
> An experiment. phkv is a **draft** format (format version 0) and may change
> without regard for existing archives. phkvar pins one commit of refkv; an
> archive written by one phkvar version may not open in another.

## Why phkv as an archive format?

| | tar | zip | phkvar |
|---|---|---|---|
| find one member | scan the whole archive | read the central directory, then search it | hash the key: a few small reads, whatever the archive's size |
| read a byte range of a member | scan to it | seek within it (if stored) | `phkvar cat -r START:END`, without decoding the rest (if stored) |
| per-member compression | no (whole stream) | deflate | none, deflate or zstd |
| duplicate files stored once | no | no | `--dedup`: later copies are references to the first |
| members that point at bytes elsewhere | no | no | `--ref`: an index of local files, pinned by size, mtime and CRC-32C |
| checksums | header only | CRC-32 | CRC-32C |
| append to an existing archive | yes | yes | **no**: the perfect hash is built when the archive is closed |

With `--zip`, phkvar writes phkv's **hybrid** profile: a valid ZIP (vzip)
archive that also carries the phkv index in a hidden entry, `__vz__/phkv`.
`unzip -l` lists it and `unzip` extracts its regular files.

## Install

```bash
cargo install --git https://github.com/mkitti/phkvar
```

## Use

```bash
phkvar create  photos.phkv ~/Pictures/trip       # alias: phkvar c
phkvar create  -z --dedup src.phkv -C ~/code proj  # zstd, duplicates stored once
phkvar create  --zip -c deflate docs.zip docs      # also a ZIP archive
phkvar create  --ref index.phkv /data/run42        # references, not copies

phkvar list    photos.phkv                         # alias: phkvar t
phkvar list -v photos.phkv trip/day1/              # long listing, under a prefix
phkvar extract photos.phkv -C restore              # alias: phkvar x
phkvar extract photos.phkv -C restore trip/day1    # just one directory
phkvar cat     photos.phkv trip/notes.txt          # one member to stdout
phkvar cat     big.phkv logs/app.log -r -4096:     # its last 4 KiB
phkvar test    photos.phkv                         # read everything, check CRCs
phkvar info    photos.phkv                         # profile, sources, metadata
```

`phkvar <command> --help` lists every option.

## How files map onto the store

| on disk | key | value | properties |
|---|---|---|---|
| regular file `a/b.txt` | `a/b.txt` | its bytes (or a reference: `--dedup`, `--ref`) | `mode`, `mtime` |
| directory `a/` | `a/` | empty | `mode`, `mtime` |
| symbolic link `a/l` | `a/l` | the link's target | `mode`, `mtime`, `github.com/mkitti/phkvar:type` = `symlink` |

`mode` (POSIX permission bits) and `mtime` (nanoseconds since the epoch) are
phkv's registered properties, so other phkv readers understand them. Paths are
stored relative: a leading `/` or `./` is dropped, and paths with `..` are
refused. Special files (devices, sockets, FIFOs) are skipped; hard links are
stored as separate files (or deduplicated with `--dedup`).

When compressing, a file that does not get smaller is stored as it is.

### Extraction safety

`extract` refuses keys that could escape the target directory (absolute
paths, `..`, empty components), never writes through an existing file or
symlink (it replaces it), and refuses to write below a directory that is a
symbolic link, so a link extracted earlier cannot redirect a later member.
References to `file://` URLs are resolved only with `--resolve-files`, since
otherwise an archive could make extraction copy any readable local file.

### Caveats of the hybrid (`--zip`) profile

ZIP tools do not understand refkv references: a reference entry's ZIP body is
its reference payload, not the file. Archives meant for plain `unzip` should
not use `--dedup` or `--ref`. ZIP entries carry no timestamps or modes from
phkvar (they read 1980-01-01); those live in the phkv index.

## Development

```bash
cargo test
cargo clippy --all-targets
```

## License

MIT OR Apache-2.0, like refkv.
