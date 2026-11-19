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
        let canonical = fs::canonicalize(checkout_path)?;
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.claim_path_of(&canonical)?)
        {
            Ok(mut file) => {
                use io::Write;
                writeln!(file, "{}", canonical.display())?;
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

    /// Why not the path as herdr reports it: herdr spells the same checkout
    /// differently across events (a created worktree as given, a reopened one
    /// with symlinks resolved), and each spelling would get its own claim.
    pub fn claim_path(&self, checkout_path: &str) -> io::Result<PathBuf> {
        self.claim_path_of(&fs::canonicalize(checkout_path)?)
    }

    fn claim_path_of(&self, canonical: &Path) -> io::Result<PathBuf> {
        let identity = checkout_identity(canonical)?;
        Ok(self
            .dir
            .join(claim_file_name(&canonical.to_string_lossy(), &identity)))
    }

    /// Why match on the recorded path instead of recomputing the file name: the
    /// name depends on the checkout's identity, which is gone with it.
    /// Why not also remove the claim of a checkout now back at that path: it
    /// was recreated after the removal, and freeing its claim would let both
    /// of its racing creation events apply the hooks. The check runs right
    /// before each removal rather than once up front, so a checkout recreated
    /// while the store is being scanned is still recognised.
    pub fn release_removed(&self, checkout_path: &str) -> io::Result<usize> {
        let removed = Path::new(checkout_path);
        let mut spellings = vec![removed.to_path_buf()];
        if let (Some(parent), Some(name)) = (removed.parent(), removed.file_name())
            && let Ok(parent) = fs::canonicalize(parent)
        {
            spellings.push(parent.join(name));
        }

        let mut count = 0;
        for entry in fs::read_dir(&self.dir)? {
            let claim = entry?.path();
            let Ok(content) = fs::read_to_string(&claim) else {
                continue;
            };
            let Some(recorded) = content.lines().next() else {
                continue;
            };
            if !spellings.iter().any(|s| s == Path::new(recorded)) {
                continue;
            }
            if self.claim_path(checkout_path).ok().as_ref() == Some(&claim) {
                continue;
            }
            match fs::remove_file(&claim) {
                Ok(()) => count += 1,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(count)
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
    fn a_checkout_reached_through_a_symlink_shares_its_claim() {
        let fx = Fixture::new();
        let checkout = fx.checkout("real/feat-x");
        let link = fx.root.path().join("link");
        std::os::unix::fs::symlink(fx.root.path().join("real"), &link).unwrap();
        let via_link = link.join("feat-x").to_string_lossy().into_owned();

        assert!(fx.store.try_acquire(&via_link).unwrap());
        assert!(!fx.store.try_acquire(&checkout).unwrap());
    }

    #[test]
    fn a_claim_records_the_canonical_checkout_path() {
        let fx = Fixture::new();
        fx.checkout("real/feat-x");
        let link = fx.root.path().join("link");
        std::os::unix::fs::symlink(fx.root.path().join("real"), &link).unwrap();
        let via_link = link.join("feat-x").to_string_lossy().into_owned();

        fx.store.try_acquire(&via_link).unwrap();

        let recorded = fs::read_to_string(fx.store.claim_path(&via_link).unwrap()).unwrap();
        assert_eq!(
            Path::new(recorded.trim_end()),
            fs::canonicalize(&via_link).unwrap()
        );
    }

    #[test]
    fn removing_a_checkout_releases_only_its_own_claims() {
        let fx = Fixture::new();
        let removed = fx.checkout("feat-x");
        let other = fx.checkout("feat-y");
        fx.store.try_acquire(&removed).unwrap();
        fx.store.try_acquire(&other).unwrap();
        let other_claim = fx.store.claim_path(&other).unwrap();
        fs::remove_dir(&removed).unwrap();

        assert_eq!(fx.store.release_removed(&removed).unwrap(), 1);
        assert_eq!(fs::read_dir(&fx.store.dir).unwrap().count(), 1);
        assert!(other_claim.exists());
    }

    #[test]
    fn a_removed_checkout_is_released_under_another_spelling_of_its_path() {
        let fx = Fixture::new();
        let checkout = fx.checkout("real/feat-x");
        fx.store.try_acquire(&checkout).unwrap();
        let link = fx.root.path().join("link");
        std::os::unix::fs::symlink(fx.root.path().join("real"), &link).unwrap();
        fs::remove_dir(&checkout).unwrap();

        let via_link = link.join("feat-x").to_string_lossy().into_owned();
        assert_eq!(fx.store.release_removed(&via_link).unwrap(), 1);
    }

    #[test]
    fn a_legacy_claim_recording_the_path_as_reported_is_released() {
        let fx = Fixture::new();
        let removed = fx
            .root
            .path()
            .join("parent-also-gone/feat-x")
            .to_string_lossy()
            .into_owned();
        fs::write(fx.store.dir.join("legacy-claim"), format!("{removed}\n")).unwrap();

        assert_eq!(fx.store.release_removed(&removed).unwrap(), 1);
    }

    #[test]
    fn the_claim_of_a_checkout_recreated_at_the_removed_path_is_kept() {
        let fx = Fixture::new();
        let checkout = fx.checkout("feat-x");
        fx.store.try_acquire(&checkout).unwrap();
        fs::remove_dir(&checkout).unwrap();
        fx.checkout("feat-x");
        let recreated = fx.store.try_acquire(&checkout).unwrap();

        let released = fx.store.release_removed(&checkout).unwrap();

        assert!(fx.store.claim_path(&checkout).unwrap().exists());
        // Without a birth time the recreated checkout may share the old claim.
        assert_eq!(released, usize::from(recreated));
    }

    #[test]
    fn entries_that_record_no_path_are_left_alone_on_removal() {
        let fx = Fixture::new();
        let removed = fx.root.path().join("gone").to_string_lossy().into_owned();
        fs::write(fx.store.dir.join("being-written"), "").unwrap();
        fs::create_dir(fx.store.dir.join("not-a-claim")).unwrap();

        assert_eq!(fx.store.release_removed(&removed).unwrap(), 0);
        assert_eq!(fs::read_dir(&fx.store.dir).unwrap().count(), 2);
    }

    #[test]
    fn a_missing_claim_store_is_reported_on_removal() {
        let fx = Fixture::new();
        fs::remove_dir(&fx.store.dir).unwrap();
        assert!(fx.store.release_removed("/nonexistent/worktree").is_err());
    }

    #[test]
    fn a_claim_that_cannot_be_removed_is_reported_on_removal() {
        use std::os::unix::fs::PermissionsExt;
        let fx = Fixture::new();
        let removed = fx.root.path().join("gone").to_string_lossy().into_owned();
        fs::write(fx.store.dir.join("claim"), format!("{removed}\n")).unwrap();
        fs::set_permissions(&fx.store.dir, fs::Permissions::from_mode(0o555)).unwrap();

        let result = fx.store.release_removed(&removed);

        fs::set_permissions(&fx.store.dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_err());
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
