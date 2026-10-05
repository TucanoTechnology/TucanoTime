//! Backup/restore (#94). `tucano-time --backup out.tar.gz` archives the whole
//! data dir (users, entries, invoices, vault store, config.json, scheduler
//! state — everything that makes an installation *itself*); `--restore` unpacks
//! it back. The archive is a plain `.tar.gz` with a `manifest.json` recording
//! app version, file list and sha256 checksums, so restores verify integrity
//! and operators can inspect with stock tools.
//!
//! Refusals protect against data loss: the live server's write lock must be
//! free, and restoring over a non-empty data dir needs `--force`. A backup
//! containing `secrets.bin` prints a reminder to supply the original vault key,
//! because without it the Settings tab will be unreadable after restore.

use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub app: String,
    pub version: String,
    pub created_at: String,
    pub files: Vec<FileEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("io: {0}")]
    Io(String),
    #[error(
        "the server holds the data-dir write lock — stop it (or scale down) before backup/restore"
    )]
    Locked,
    #[error("destination is not empty; pass --force to overwrite")]
    NotEmpty,
    #[error("archive integrity check failed: {0}")]
    Corrupt(String),
    #[error("unsafe path in archive: {0}")]
    UnsafePath(String),
}

impl From<std::io::Error> for BackupError {
    fn from(e: std::io::Error) -> Self {
        BackupError::Io(e.to_string())
    }
}

/// Collect every regular file under `root` as (relative path, absolute path),
/// sorted and skipping tmp/lock artefacts.
fn walk(root: &Path) -> Result<Vec<(String, PathBuf)>, BackupError> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy().to_string();
            if path.is_dir() {
                // `.restoring` (and any dot-dir) is transient machinery.
                if !name.starts_with('.') {
                    stack.push(path);
                }
                continue;
            }
            // Transient/runtime files are never part of a backup. Dot files
            // like `invoices/.seq.json` ARE kept — the numbering counter must
            // survive a restore too (review B3/D5).
            if name.ends_with(".tmp") || name.ends_with(".lock") {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .map_err(|_| BackupError::Io("walk escaped root".into()))?
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            out.push((rel, path));
        }
    }
    out.sort();
    Ok(out)
}

fn finalize_hex(h: &Sha256) -> String {
    h.clone()
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Stream-hash a file (no whole-file read).
fn hash_file(path: &Path) -> Result<(u64, String), BackupError> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok((f.metadata()?.len(), finalize_hex(&h)))
}

/// Take the store's lock file without blocking and **hold it** for the
/// duration (returned `File` releases on drop). The server grabs this same
/// lock briefly around every atomic write, so holding it means: refuse if a
/// write is in flight right now, and block any write that starts while we
/// snapshot — a point-in-time, cross-file-consistent backup.
fn acquire_lock(root: &Path) -> Result<std::fs::File, BackupError> {
    use fs2::FileExt;
    let lock = root.join(".tucanotime.lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock)
        .map_err(|e| BackupError::Io(e.to_string()))?;
    f.try_lock_exclusive().map_err(|_| BackupError::Locked)?;
    Ok(f)
}

/// Create `out.tar.gz` from the data dir. Returns the manifest file count.
pub fn create(root: &Path, out: &Path) -> Result<usize, BackupError> {
    if !root.is_dir() {
        return Err(BackupError::Io(format!(
            "data dir {} does not exist",
            root.display()
        )));
    }
    // Lock or fail: never snapshot through an in-flight write, and hold off
    // new writes for the duration of the copy.
    let _lock = acquire_lock(root)?;
    // Two streaming passes (hash, then copy): the archive is never held in
    // memory (review D5) — the lock guarantees nothing changes in between.
    let files = walk(root)?;
    let mut entries = Vec::new();
    for (rel, abs) in &files {
        let (size, digest) = hash_file(abs)?;
        entries.push(FileEntry {
            path: rel.clone(),
            size,
            sha256: digest,
        });
    }
    let manifest = Manifest {
        app: "tucano-time".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        created_at: chrono::Utc::now().to_rfc3339(),
        files: entries,
    };
    let manifest_json =
        serde_json::to_vec_pretty(&manifest).map_err(|e| BackupError::Io(e.to_string()))?;

    let file = std::fs::File::create(out)?;
    let mut gz = GzEncoder::new(file, Compression::default());
    tar_write(&mut gz, "manifest.json", &manifest_json)?;
    for (rel, abs) in &files {
        let mut f = std::fs::File::open(abs)?;
        let size = f.metadata()?.len();
        tar_write_stream(&mut gz, rel, &mut f, size)?;
    }
    tar_finish(&mut gz)?;
    gz.finish()?;
    Ok(files.len())
}

/// Restore `in.tar.gz` into `target` (the data dir). Returns operator-facing
/// notes (vault reminder etc.).
///
/// Streaming (review D5): the manifest must be the FIRST entry; each body is
/// copied into a staging folder while hashing, verified against the manifest,
/// then moved into place — memory stays O(1) per file.
pub fn restore(in_path: &Path, target: &Path, force: bool) -> Result<Vec<String>, BackupError> {
    if target.exists() && !force && std::fs::read_dir(target)?.next().is_some() {
        return Err(BackupError::NotEmpty);
    }
    // Never unpack under a live server: take the same lock `create` uses.
    let _lock = if target.exists() {
        Some(acquire_lock(target)?)
    } else {
        None
    };
    std::fs::create_dir_all(target)?;
    let staging = target.join(".restoring");
    let _ = std::fs::remove_dir_all(&staging); // stale leftovers from a crash
    std::fs::create_dir_all(&staging)?;

    let src = std::fs::File::open(in_path)?;
    let mut gz = GzDecoder::new(src);
    let mut notes = Vec::new();

    // 1) manifest (first entry)
    let manifest = match tar_read(&mut gz)? {
        Some((name, bytes)) if sanitize(&name)? == "manifest.json" => {
            serde_json::from_slice::<Manifest>(&bytes)
                .map_err(|e| BackupError::Corrupt(e.to_string()))?
        }
        Some(_) => {
            return Err(BackupError::Corrupt("manifest.json must be first".into()));
        }
        None => return Err(BackupError::Corrupt("empty archive".into())),
    };
    let mut seen = std::collections::HashSet::new();
    let mut had_vault = false;

    // 2) stream + verify each body
    let result = (|| -> Result<(), BackupError> {
        while let Some((name, mut copy)) = tar_read_stream(&mut gz)? {
            let rel = sanitize(&name)?;
            let entry = manifest
                .files
                .iter()
                .find(|f| f.path == rel)
                .ok_or_else(|| BackupError::Corrupt(format!("{rel}: not in manifest")))?;
            let dest = staging.join(&rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut f = std::fs::File::create(&dest)?;
            let mut h = Sha256::new();
            std::io::copy(&mut TeeReader(&mut h, &mut copy), &mut f)
                .map_err(|e| BackupError::Io(e.to_string()))?;
            let digest = finalize_hex(&h);
            if digest != entry.sha256 {
                return Err(BackupError::Corrupt(format!(
                    "{rel}: sha256 mismatch (archive is older than manifest?)"
                )));
            }
            if rel == "secrets.bin" {
                had_vault = true;
                notes.push(
                    "backup contains the encrypted vault store (secrets.bin): the original \
                     TUCANO_SECRET_KEY(_FILE) must be configured or the Settings tab will be \
                     unreadable"
                        .into(),
                );
            }
            seen.insert(rel);
        }
        for entry in &manifest.files {
            if !seen.contains(&entry.path) {
                return Err(BackupError::Corrupt(format!(
                    "{} missing from archive",
                    entry.path
                )));
            }
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }

    // 3) publish verified files
    for entry in &manifest.files {
        let from = staging.join(&entry.path);
        let to = target.join(&entry.path);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&from, &to)?;
    }
    let _ = std::fs::remove_dir_all(&staging);

    // Operator nudge: a vault store without its key is unreadable on boot.
    let key_present = std::env::var("TUCANO_SECRET_KEY")
        .ok()
        .or_else(|| std::env::var("TUCANO_SECRET_KEY_FILE").ok())
        .is_some_and(|v| !v.is_empty());
    if !key_present && had_vault {
        notes.push(
            "WARNING: no TUCANO_SECRET_KEY(_FILE) is set in this environment, but the \
             backup carries a vault store."
                .into(),
        );
    }
    Ok(notes)
}

/// One streamed archive entry: name + seekable body cursor.
type ArchiveEntry = (String, std::io::Cursor<Vec<u8>>);

/// Read one tar entry (name + body). The body is held one-file-at-a-time —
/// memory is O(largest file), not O(whole data dir) as before (review D5).
fn tar_read_stream<R: Read>(r: &mut R) -> Result<Option<ArchiveEntry>, BackupError> {
    let mut header = [0u8; 512];
    if !read_exact_or_eof(r, &mut header)? {
        return Ok(None);
    }
    if header.iter().all(|b| *b == 0) {
        return Ok(None);
    }
    let name = cstr(&header[0..100]);
    let size = octal(&header[124..136])? as usize;
    let pad = (512 - (size % 512)) % 512;
    // Caller consumes `size` bytes; we leave body in the stream, and the pad
    // is skipped after the copy. To keep the API honest we wrap the reader:
    // copy exactly size, then consume pad.
    let mut limiter = r.take(size as u64);
    let mut body = Vec::new();
    std::io::Read::read_to_end(&mut limiter, &mut body)
        .map_err(|e| BackupError::Io(e.to_string()))?;
    let mut discard = [0u8; 512];
    let mut left = pad;
    while left > 0 {
        let n = left.min(512);
        r.read_exact(&mut discard[..n])?;
        left -= n;
    }
    Ok(Some((name, std::io::Cursor::new(body))))
}

/// Tee bytes through a hasher while copying.
struct TeeReader<'a, H: sha2::Digest, R>(&'a mut H, &'a mut R);
impl<R: Read, H: sha2::Digest> Read for TeeReader<'_, H, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.1.read(buf)?;
        self.0.update(&buf[..n]);
        Ok(n)
    }
}

/// Reject absolute paths and any `..` component; keep plain relative paths.
fn sanitize(name: &str) -> Result<String, BackupError> {
    let p = Path::new(name);
    if p.is_absolute() {
        return Err(BackupError::UnsafePath(name.into()));
    }
    for c in p.components() {
        if matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(BackupError::UnsafePath(name.into()));
        }
    }
    Ok(name.to_string())
}

// --------------------------------------------------------- minimal tar ustar --
// 512-byte headers; regular files only; paths are short by construction (the
// data-dir layout is shallow). Private to this module, round-trip tested.
// ustar field layout: name 0..100, mode 100..108, uid 108..116, gid 116..124,
// size 124..136, mtime 136..148, chksum 148..156, typeflag 156, linkname
// 157..257, magic 257..263 ("ustar\0"), version 263..265 ("00").

fn tar_header<W: Write>(w: &mut W, name: &str, size: u64) -> Result<(), BackupError> {
    if name.len() > 99 {
        return Err(BackupError::Io(format!("path too long for tar: {name}")));
    }
    let mut header = [b' '; 512];
    header[..name.len()].copy_from_slice(name.as_bytes());
    write_octal(&mut header[100..108], 0o644);
    write_octal(&mut header[108..116], 0);
    write_octal(&mut header[116..124], 0);
    write_octal(&mut header[124..136], size);
    write_octal(&mut header[136..148], chrono::Utc::now().timestamp() as u64);
    header[156] = b'0'; // regular file
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    // The checksum covers the header with its own field read as spaces (b' '
    // is already the fill value).
    let sum: u32 = header.iter().map(|b| *b as u32).sum();
    let cs = format!("{sum:06o}\0 ");
    header[148..156].copy_from_slice(cs.as_bytes());
    w.write_all(&header)?;
    Ok(())
}

fn write_octal(field: &mut [u8], value: u64) {
    let n = field.len();
    // (n-1) digits of zero-padded octal, then a NUL terminator.
    let s = format!("{value:0>width$o}", width = n - 1);
    let bytes = s.as_bytes();
    let take = bytes.len().min(n - 1);
    field[..n - 1 - take].fill(b'0');
    field[n - 1 - take..n - 1].copy_from_slice(&bytes[bytes.len() - take..]);
    field[n - 1] = b'\0';
}

/// Tar entry sourced from a reader — the archive never holds a file body.
fn tar_write_stream<W: Write, R: Read>(
    w: &mut W,
    name: &str,
    r: &mut R,
    size: u64,
) -> Result<(), BackupError> {
    tar_header(w, name, size)?;
    let mut limiter = r.take(size);
    std::io::copy(&mut limiter, w).map_err(|e| BackupError::Io(e.to_string()))?;
    let pad = (512 - (size % 512)) % 512;
    if pad > 0 {
        w.write_all(&vec![0u8; pad as usize])?;
    }
    Ok(())
}

fn tar_write<W: Write>(w: &mut W, name: &str, bytes: &[u8]) -> Result<(), BackupError> {
    tar_header(w, name, bytes.len() as u64)?;
    w.write_all(bytes)?;
    let pad = (512 - (bytes.len() % 512)) % 512;
    if pad > 0 {
        w.write_all(&vec![0u8; pad])?;
    }
    Ok(())
}

fn tar_finish<W: Write>(w: &mut W) -> Result<(), BackupError> {
    w.write_all(&[0u8; 1024])?; // two zero blocks terminate the archive
    Ok(())
}

fn tar_read<R: Read>(r: &mut R) -> Result<Option<(String, Vec<u8>)>, BackupError> {
    let mut header = [0u8; 512];
    if !read_exact_or_eof(r, &mut header)? {
        return Ok(None);
    }
    if header.iter().all(|b| *b == 0) {
        return Ok(None); // end-of-archive block
    }
    let name = cstr(&header[0..100]);
    let size = octal(&header[124..136])? as usize;
    let mut bytes = vec![0u8; size];
    r.read_exact(&mut bytes)?;
    let pad = (512 - (size % 512)) % 512;
    let mut discard = [0u8; 512];
    if pad > 0 {
        r.read_exact(&mut discard[..pad])?;
    }
    Ok(Some((name, bytes)))
}

fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<bool, BackupError> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let n = r.read(&mut buf[filled..])?;
        if n == 0 {
            return Ok(filled != 0); // partial header is corruption; none is EOF
        }
        filled += n;
    }
    Ok(true)
}

/// Tar text fields are NUL-terminated *and* space-padded; trim both.
fn cstr(field: &[u8]) -> String {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end])
        .trim_end_matches(' ')
        .to_string()
}

fn octal(field: &[u8]) -> Result<u64, BackupError> {
    let s = cstr(field);
    let s = s.trim();
    if s.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(s, 8).map_err(|_| BackupError::Corrupt(format!("bad octal {s:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_data(dir: &Path) {
        std::fs::create_dir_all(dir.join("invoices")).unwrap();
        std::fs::write(dir.join("users.json"), b"[{\"id\":\"a\"}]").unwrap();
        std::fs::write(dir.join("invoices/INV-1.json"), b"{\"total\":99}").unwrap();
        std::fs::write(dir.join("secrets.bin"), b"encrypted blob").unwrap();
        std::fs::write(dir.join("config.json"), b"{\"reminder_days\":10}").unwrap();
        // transient file must be skipped
        std::fs::write(dir.join("users.json.tmp"), b"partial").unwrap();
    }

    #[test]
    fn archived_pdf_round_trips_with_intact_sha256() {
        // #113: the archive under invoices/ is a non-JSON file the backup
        // walk already covers; prove the PDF bytes survive untouched and the
        // manifest checksum matches the original.
        use sha2::{Digest, Sha256};
        let src = tempfile::tempdir().unwrap();
        let hold = tempfile::tempdir().unwrap();
        seed_data(src.path());
        let pdf = b"%PDF-1.4\n\x80\x81\xfe rest of the invoice document".to_vec();
        std::fs::write(src.path().join("invoices/INV-1.pdf"), &pdf).unwrap();
        let original_sha = Sha256::digest(&pdf);

        let archive = hold.path().join("out.tar.gz");
        create(src.path(), &archive).unwrap();
        let dst = tempfile::tempdir().unwrap();
        restore(&archive, dst.path(), false).unwrap();

        let restored = std::fs::read(dst.path().join("invoices/INV-1.pdf")).unwrap();
        assert_eq!(restored, pdf, "PDF bytes round-trip byte-for-byte");
        assert_eq!(Sha256::digest(&restored), original_sha);
        // `restore()` verifies each file's manifest sha256 as it streams (it
        // returns Corrupt on any mismatch), so a clean restore is the proof
        // the archived PDF's checksum survived the round-trip.
        let _ = &hold;
    }

    #[test]
    fn backup_restore_roundtrip_with_checksums() {
        let src = tempfile::tempdir().unwrap();
        let hold = tempfile::tempdir().unwrap(); // archive lives outside the backup root
        seed_data(src.path());
        let archive = hold.path().join("out.tar.gz");
        let n = create(src.path(), &archive).unwrap();
        assert_eq!(n, 4, "tmp file excluded");

        let dst = tempfile::tempdir().unwrap();
        let notes = restore(&archive, dst.path(), false).unwrap();
        assert!(
            std::fs::read_to_string(dst.path().join("users.json"))
                .unwrap()
                .contains("a")
        );
        assert!(
            std::fs::read_to_string(dst.path().join("invoices/INV-1.json"))
                .unwrap()
                .contains("99")
        );
        assert_eq!(
            std::fs::read(dst.path().join("secrets.bin")).unwrap(),
            b"encrypted blob"
        );
        assert!(!dst.path().join("users.json.tmp").exists());
        assert!(notes.iter().any(|x| x.contains("secrets.bin")));
        let _ = &hold;

        // Restore over a non-empty dir is refused without --force.
        assert!(matches!(
            restore(&archive, dst.path(), false),
            Err(BackupError::NotEmpty)
        ));
        assert!(restore(&archive, dst.path(), true).is_ok());
        let _ = std::fs::remove_file(&archive);
    }

    #[test]
    fn tampered_archive_fails_verification() {
        let src = tempfile::tempdir().unwrap();
        seed_data(src.path());
        let archive = src.path().join("out.tar.gz");
        create(src.path(), &archive).unwrap();
        // Flip a byte inside the payload region (past headers of file #2).
        let bytes = std::fs::read(&archive).unwrap();
        // Re-pack raw: decode gz, edit, re-encode.
        let mut plain = Vec::new();
        GzDecoder::new(bytes.as_slice())
            .read_to_end(&mut plain)
            .unwrap();
        let pos = plain
            .windows(b"\"total\":99".len())
            .position(|w| w == b"\"total\":99")
            .unwrap();
        plain[pos + 9] = b'8'; // 99 -> 98
        let mut enc = std::io::Cursor::new(Vec::new());
        {
            let mut g = GzEncoder::new(&mut enc, Compression::default());
            g.write_all(&plain).unwrap();
        }
        std::fs::write(&archive, enc.into_inner()).unwrap();
        let dst = tempfile::tempdir().unwrap();
        assert!(matches!(
            restore(&archive, dst.path(), false),
            Err(BackupError::Corrupt(_))
        ));
    }

    #[test]
    fn path_traversal_in_archive_is_rejected() {
        assert!(sanitize("a/b.json").is_ok());
        assert!(matches!(
            sanitize("../evil"),
            Err(BackupError::UnsafePath(_))
        ));
        assert!(matches!(
            sanitize("/etc/passwd"),
            Err(BackupError::UnsafePath(_))
        ));
    }
}
