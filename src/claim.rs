use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
            .open(self.claim_path(checkout_path)?)
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
        match fs::remove_file(self.claim_path(checkout_path)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub fn claim_path(&self, checkout_path: &str) -> io::Result<PathBuf> {
        let identity = checkout_identity(Path::new(checkout_path))?;
        Ok(self.dir.join(claim_file_name(checkout_path, &identity)))
    }
}

/// Why not the path alone: a worktree removed and recreated at the same path
/// would inherit the old claim and be skipped.
/// Why not mtime/ctime: copying into the checkout root changes them between
/// the two creation events that race for the claim, so both would win.
/// Why not fall back to the path on a stat error: racers that disagree on the
/// key would both acquire, so the error is surfaced instead.
/// Birth time is unavailable on musl builds; dev+ino alone may then be reused
/// by a recreated checkout, which degrades to a skip, never a double apply.
fn checkout_identity(checkout: &Path) -> io::Result<String> {
    let metadata = fs::metadata(checkout)?;
    Ok(identity_of(
        metadata.dev(),
        metadata.ino(),
        metadata.created().ok(),
    ))
}

fn identity_of(dev: u64, ino: u64, birth: Option<SystemTime>) -> String {
    let birth = birth
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or_else(|| "-".to_string(), |d| d.as_nanos().to_string());
    format!("{dev}:{ino}:{birth}")
}

/// Why not a plain sanitized path: distinct checkouts can sanitize to the same
/// string (`/a/b` and `/a_b`), so a hash of the original keeps them apart.
fn claim_file_name(checkout_path: &str, identity: &str) -> String {
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
    let key = format!("{checkout_path}\0{identity}");
    format!("{label}-{:016x}", fnv1a64(key.as_bytes()))
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

    struct Fixture {
        _state: tempfile::TempDir,
        root: tempfile::TempDir,
        store: ClaimStore,
    }

    impl Fixture {
        fn new() -> Self {
            let state = tempfile::tempdir().unwrap();
            let store = ClaimStore::new(state.path()).unwrap();
            Self {
                _state: state,
                root: tempfile::tempdir().unwrap(),
                store,
            }
        }

        fn checkout(&self, relative: &str) -> String {
            let path = self.root.path().join(relative);
            fs::create_dir_all(&path).unwrap();
            path.to_string_lossy().into_owned()
        }
    }

    #[test]
    fn first_acquire_wins_and_second_is_refused() {
        let fx = Fixture::new();
        let checkout = fx.checkout("feat-x");
        assert!(fx.store.try_acquire(&checkout).unwrap());
        assert!(!fx.store.try_acquire(&checkout).unwrap());
    }

    #[test]
    fn writing_into_the_checkout_does_not_free_its_claim() {
        let fx = Fixture::new();
        let checkout = fx.checkout("feat-x");
        assert!(fx.store.try_acquire(&checkout).unwrap());
        fs::write(Path::new(&checkout).join(".env"), "copied").unwrap();
        assert!(!fx.store.try_acquire(&checkout).unwrap());
    }

    #[test]
    fn a_checkout_recreated_at_the_same_path_is_claimed_anew() {
        let fx = Fixture::new();
        let checkout = fx.checkout("feat-x");
        let before = checkout_identity(Path::new(&checkout)).unwrap();
        assert!(fx.store.try_acquire(&checkout).unwrap());

        fs::remove_dir(&checkout).unwrap();
        fx.checkout("feat-x");

        // Without a birth time the filesystem may hand back the same inode,
        // and then the checkouts are indistinguishable by design.
        let recreated = checkout_identity(Path::new(&checkout)).unwrap() != before;
        assert_eq!(fx.store.try_acquire(&checkout).unwrap(), recreated);
    }

    #[test]
    fn releasing_after_failure_allows_a_retry() {
        let fx = Fixture::new();
        let checkout = fx.checkout("feat-x");
        fx.store.try_acquire(&checkout).unwrap();
        fx.store.release(&checkout).unwrap();
        assert!(fx.store.try_acquire(&checkout).unwrap());
    }

    #[test]
    fn releasing_an_unclaimed_checkout_is_not_an_error() {
        let fx = Fixture::new();
        assert!(fx.store.release(&fx.checkout("never-claimed")).is_ok());
    }

    #[test]
    fn a_missing_checkout_cannot_be_claimed() {
        let fx = Fixture::new();
        assert!(fx.store.try_acquire("/nonexistent/worktree").is_err());
    }

    #[test]
    fn an_unwritable_claim_store_is_reported_instead_of_read_as_already_claimed() {
        let fx = Fixture::new();
        let checkout = fx.checkout("feat-x");
        fs::remove_dir(&fx.store.dir).unwrap();
        assert!(fx.store.try_acquire(&checkout).is_err());
    }

    #[test]
    fn a_claim_that_cannot_be_removed_is_reported_on_release() {
        let fx = Fixture::new();
        let checkout = fx.checkout("feat-x");
        fs::create_dir(fx.store.claim_path(&checkout).unwrap()).unwrap();
        assert!(fx.store.release(&checkout).is_err());
    }

    #[test]
    fn a_missing_checkout_cannot_be_released() {
        let fx = Fixture::new();
        assert!(fx.store.release("/nonexistent/worktree").is_err());
    }

    #[test]
    fn without_a_birth_time_the_identity_falls_back_to_device_and_inode() {
        assert_eq!(identity_of(1, 2, None), "1:2:-");
        assert_ne!(
            identity_of(1, 2, None),
            identity_of(1, 2, Some(UNIX_EPOCH + std::time::Duration::from_nanos(5)))
        );
    }

    #[test]
    fn checkouts_that_sanitize_alike_still_claim_independently() {
        let fx = Fixture::new();
        let nested = fx.checkout("a/b");
        let flat = fx.checkout("a_b");
        assert!(fx.store.try_acquire(&nested).unwrap());
        assert!(fx.store.try_acquire(&flat).unwrap());
        assert!(!fx.store.try_acquire(&nested).unwrap());
        assert!(!fx.store.try_acquire(&flat).unwrap());
    }

    #[test]
    fn the_same_path_with_another_identity_gets_another_claim() {
        assert_ne!(
            claim_file_name("/repos/feat-x", "1:2:100"),
            claim_file_name("/repos/feat-x", "1:3:200")
        );
    }

    #[test]
    fn the_same_path_and_identity_always_map_to_the_same_claim() {
        assert_eq!(
            claim_file_name("/repos/feat-x", "1:2:100"),
            claim_file_name("/repos/feat-x", "1:2:100")
        );
    }

    #[test]
    fn very_long_checkout_paths_stay_within_filename_limits() {
        let long = format!("/repos/{}", "segment/".repeat(60));
        assert!(claim_file_name(&long, "1:2:100").len() <= 100);
    }
}
