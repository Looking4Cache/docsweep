//! Writes invoice files so that a stored file is provably complete.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

/// Attempts to find a free name before giving up.
const MAX_NAME_ATTEMPTS: u32 = 1000;

/// Why a file could not be stored.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("cannot write to {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the file written to {path} does not match the original")]
    VerificationFailed { path: PathBuf },
    #[error("no free file name found for {path}")]
    NoFreeName { path: PathBuf },
}

/// Lowercase hexadecimal SHA-256 of some bytes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hash = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        hash.push_str(&format!("{byte:02x}"));
    }
    hash
}

/// The path a file would get: `name.pdf`, or `name (2).pdf` if that is taken.
pub fn free_path(dir: &Path, filename: &str) -> Result<PathBuf, StorageError> {
    candidates(dir, filename)
        .find(|path| !path.exists())
        .ok_or_else(|| StorageError::NoFreeName {
            path: dir.join(filename),
        })
}

fn candidates(dir: &Path, filename: &str) -> impl Iterator<Item = PathBuf> {
    let (stem, extension) = filename.rsplit_once('.').unwrap_or((filename, ""));
    let (stem, extension, dir) = (stem.to_owned(), extension.to_owned(), dir.to_path_buf());
    let first = dir.join(filename);
    std::iter::once(first).chain((2..=MAX_NAME_ATTEMPTS).map(move |n| {
        dir.join(if extension.is_empty() {
            format!("{stem} ({n})")
        } else {
            format!("{stem} ({n}).{extension}")
        })
    }))
}

/// Stores bytes under a free name and returns the final path.
///
/// The data goes to a temporary file in the target directory, is flushed to
/// disk and read back for comparison, and only then gets its final name. An
/// existing file is never overwritten, and a crash leaves no partial invoice.
pub fn store_verified(dir: &Path, filename: &str, bytes: &[u8]) -> Result<PathBuf, StorageError> {
    let io_error = |path: &Path| {
        let path = path.to_path_buf();
        move |source| StorageError::Io { path, source }
    };

    fs::create_dir_all(dir).map_err(io_error(dir))?;
    let mut temporary = NamedTempFile::new_in(dir).map_err(io_error(dir))?;
    temporary.write_all(bytes).map_err(io_error(dir))?;
    temporary.as_file().sync_all().map_err(io_error(dir))?;

    let written = fs::read(temporary.path()).map_err(io_error(temporary.path()))?;
    if written != bytes {
        return Err(StorageError::VerificationFailed {
            path: temporary.path().to_path_buf(),
        });
    }

    let mut temporary = temporary;
    for candidate in candidates(dir, filename) {
        match temporary.persist_noclobber(&candidate) {
            Ok(_) => return Ok(candidate),
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                temporary = error.file;
            }
            Err(error) => {
                return Err(StorageError::Io {
                    path: candidate,
                    source: error.error,
                });
            }
        }
    }
    Err(StorageError::NoFreeName {
        path: dir.join(filename),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_matches_known_value() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn stores_the_exact_bytes_and_creates_directories() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a").join("b");
        let path = store_verified(&target, "x.pdf", b"content").unwrap();
        assert_eq!(path, target.join("x.pdf"));
        assert_eq!(fs::read(path).unwrap(), b"content");
    }

    #[test]
    fn never_overwrites_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let first = store_verified(dir.path(), "x.pdf", b"one").unwrap();
        let second = store_verified(dir.path(), "x.pdf", b"two").unwrap();
        assert_eq!(second, dir.path().join("x (2).pdf"));
        assert_eq!(fs::read(first).unwrap(), b"one");
        assert_eq!(fs::read(second).unwrap(), b"two");
    }

    #[test]
    fn leaves_no_temporary_files_behind() {
        let dir = tempfile::tempdir().unwrap();
        store_verified(dir.path(), "x.pdf", b"one").unwrap();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn free_path_predicts_the_stored_name() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            free_path(dir.path(), "x.pdf").unwrap(),
            dir.path().join("x.pdf")
        );
        store_verified(dir.path(), "x.pdf", b"one").unwrap();
        let predicted = free_path(dir.path(), "x.pdf").unwrap();
        assert_eq!(
            predicted,
            store_verified(dir.path(), "x.pdf", b"two").unwrap()
        );
    }

    #[test]
    fn reports_a_blocked_target_directory() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("file");
        fs::write(&blocker, b"x").unwrap();
        assert!(matches!(
            store_verified(&blocker, "x.pdf", b"y"),
            Err(StorageError::Io { .. })
        ));
    }
}
