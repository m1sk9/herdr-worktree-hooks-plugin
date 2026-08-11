use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// One file per worktree checkout, created with `O_EXCL` so that overlapping
/// events race on the filesystem instead of on a lock we would have to hold.
pub struct ClaimStore {
    dir: PathBuf,
}

impl ClaimStore {
    pub fn new(state_dir: &Path) -> io::Result<Self> {
        let dir = state_dir.join("claims");
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    pub fn try_acquire(&self, checkout_path: &str) -> io::Result<bool> {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.path_for(checkout_path))
        {
            Ok(mut file) => {
                use io::Write;
                writeln!(file, "{checkout_path}")?;
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub fn release(&self, checkout_path: &str) -> io::Result<()> {
        match fs::remove_file(self.path_for(checkout_path)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn path_for(&self, checkout_path: &str) -> PathBuf {
        self.dir.join(claim_file_name(checkout_path))
    }
}

/// Why not a plain sanitized path: distinct checkouts can sanitize to the same
/// string (`/a/b` and `/a_b`), so a hash of the original keeps them apart.
fn claim_file_name(checkout_path: &str) -> String {
    let mut label: String = checkout_path
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = label.len().saturating_sub(80);
    label = label.split_off(trimmed);
    format!("{label}-{:016x}", fnv1a64(checkout_path.as_bytes()))
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, ClaimStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ClaimStore::new(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn first_acquire_wins_and_second_is_refused() {
        let (_dir, store) = store();
        assert!(
            store
                .try_acquire("/home/dev/src/worktrees/app/feat-x")
                .unwrap()
        );
        assert!(
            !store
                .try_acquire("/home/dev/src/worktrees/app/feat-x")
                .unwrap()
        );
    }

    #[test]
    fn releasing_after_failure_allows_a_retry() {
        let (_dir, store) = store();
        store
            .try_acquire("/home/dev/src/worktrees/app/feat-x")
            .unwrap();
        store.release("/home/dev/src/worktrees/app/feat-x").unwrap();
        assert!(
            store
                .try_acquire("/home/dev/src/worktrees/app/feat-x")
                .unwrap()
        );
    }

    #[test]
    fn releasing_an_unclaimed_checkout_is_not_an_error() {
        let (_dir, store) = store();
        assert!(store.release("/never/claimed").is_ok());
    }

    #[test]
    fn checkouts_that_sanitize_alike_still_claim_independently() {
        let (_dir, store) = store();
        assert!(store.try_acquire("/repos/a/b").unwrap());
        assert!(store.try_acquire("/repos/a_b").unwrap());
        assert!(!store.try_acquire("/repos/a/b").unwrap());
        assert!(!store.try_acquire("/repos/a_b").unwrap());
    }

    #[test]
    fn very_long_checkout_paths_stay_within_filename_limits() {
        let long = format!("/repos/{}", "segment/".repeat(60));
        assert!(claim_file_name(&long).len() <= 100);
    }
}
