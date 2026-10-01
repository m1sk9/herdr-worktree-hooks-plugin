use std::ffi::OsString;
use std::fmt::Display;
use std::fs::FileType;
use std::io::{self, ErrorKind};
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::event::WorktreeTarget;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CopyReport {
    pub copied: Vec<String>,
    pub skipped_existing: Vec<String>,
    pub skipped_special: Vec<String>,
    pub skipped_too_large: Vec<String>,
    pub skipped_repositories: Vec<String>,
    pub missing_source: Vec<String>,
}

type FileId = (u64, u64);

#[derive(Debug)]
enum Failure {
    /// The source was removed from main after it was listed.
    Vanished,
    Other(String),
}

impl Failure {
    /// Why only reads of main: a NotFound while writing the worktree is not a
    /// file that went away, and must not be passed off as one.
    fn reading(e: io::Error, context: impl Display) -> Self {
        match e.kind() {
            ErrorKind::NotFound => Self::Vanished,
            _ => Self::Other(format!("{context}: {e}")),
        }
    }

    fn writing(e: io::Error, context: impl Display) -> Self {
        Self::Other(format!("{context}: {e}"))
    }
}

/// Copies each entry from the main checkout into the worktree, merging
/// directories file by file. Anything that already exists in the worktree is
/// left untouched — the whole point is to seed a fresh checkout, not to
/// overwrite local edits.
///
/// A directory holding more than `max_files` files is skipped as a whole
/// rather than failing the run: a failure releases the claim, so every later
/// event would hit the same limit again and `run` would never get to rebuild it.
/// For the same reason, a file that vanishes from main mid-walk (a build
/// cleaning up after itself) is reported as missing instead of failing.
pub fn copy_files(
    main: &Path,
    worktree: &Path,
    entries: &[String],
    max_files: usize,
) -> Result<CopyReport, String> {
    let mut report = CopyReport::default();
    let worktree_id = std::fs::metadata(worktree)
        .map(|m| (m.dev(), m.ino()))
        .map_err(|e| format!("{}: {e}", worktree.display()))?;

    for entry in entries {
        let relative = validate_entry(entry)?;
        let source = main.join(&relative);
        let destination = worktree.join(&relative);

        if has_symlinked_ancestor(worktree, &relative) {
            report.skipped_existing.push(entry.clone());
            continue;
        }

        // `metadata` follows a symlinked entry on purpose (like `cp -RH`): the
        // entry names what should be seeded, not how main happens to store it.
        let Ok(metadata) = source.metadata() else {
            report.missing_source.push(entry.clone());
            continue;
        };

        // Why count before copying: stopping halfway would leave a partial tree.
        if metadata.is_dir() && exceeds(&source, max_files, worktree_id)? {
            report.skipped_too_large.push(entry.clone());
            continue;
        }

        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let file_type = metadata.file_type();
        seed(
            &source,
            &destination,
            Path::new(entry),
            file_type,
            worktree_id,
            &mut report,
        )?;
    }

    Ok(report)
}

fn seed(
    source: &Path,
    destination: &Path,
    label: &Path,
    file_type: FileType,
    worktree_id: FileId,
    report: &mut CopyReport,
) -> Result<(), String> {
    let seeded = if file_type.is_dir() {
        copy_dir(source, destination, label, worktree_id, report)
    } else {
        copy_leaf(source, destination, label, file_type, report)
    };
    match seeded {
        Ok(()) => Ok(()),
        Err(Failure::Vanished) => {
            report
                .missing_source
                .push(label.to_string_lossy().into_owned());
            Ok(())
        }
        Err(Failure::Other(message)) => Err(message),
    }
}

fn copy_dir(
    source: &Path,
    destination: &Path,
    label: &Path,
    worktree_id: FileId,
    report: &mut CopyReport,
) -> Result<(), Failure> {
    let metadata = std::fs::metadata(source).map_err(|e| Failure::reading(e, source.display()))?;
    // Why not descend: a worktree nested inside the main checkout would be
    // copied into itself, growing while it is being walked.
    if (metadata.dev(), metadata.ino()) == worktree_id {
        return Ok(());
    }

    let created = match destination.symlink_metadata() {
        Ok(existing) if existing.is_dir() => false,
        // Why not merge through it: a file or symlink at this path belongs to
        // the worktree, and following a symlink could write outside of it.
        Ok(_) => {
            report
                .skipped_existing
                .push(label.to_string_lossy().into_owned());
            return Ok(());
        }
        Err(_) => {
            std::fs::create_dir(destination)
                .map_err(|e| Failure::writing(e, destination.display()))?;
            true
        }
    };

    let children = list_children(source).map_err(|e| Failure::reading(e, source.display()))?;
    for (name, file_type) in children {
        let from = source.join(&name);
        let to = destination.join(&name);
        let label = label.join(&name);

        if file_type.is_dir() && is_repository(&from) {
            report
                .skipped_repositories
                .push(label.to_string_lossy().into_owned());
        } else {
            seed(&from, &to, &label, file_type, worktree_id, report).map_err(Failure::Other)?;
        }
    }

    // Why not right after `create_dir`: a read-only source mode would block
    // creating the children.
    if created {
        std::fs::set_permissions(destination, metadata.permissions())
            .map_err(|e| Failure::writing(e, destination.display()))?;
    }
    Ok(())
}

fn copy_leaf(
    source: &Path,
    destination: &Path,
    label: &Path,
    file_type: FileType,
    report: &mut CopyReport,
) -> Result<(), Failure> {
    let label = label.to_string_lossy().into_owned();
    if destination.symlink_metadata().is_ok() {
        report.skipped_existing.push(label);
        return Ok(());
    }

    if file_type.is_symlink() {
        // Why not follow: a link inside a directory can form a cycle or reach
        // outside the repository; recreated verbatim it can do neither. The cost
        // is that an absolute link into main stays shared with main.
        let target =
            std::fs::read_link(source).map_err(|e| Failure::reading(e, source.display()))?;
        symlink(&target, destination).map_err(|e| Failure::writing(e, destination.display()))?;
    } else if file_type.is_file() {
        std::fs::copy(source, destination).map_err(|e| {
            Failure::reading(
                e,
                format_args!("{} -> {}", source.display(), destination.display()),
            )
        })?;
    } else {
        report.skipped_special.push(label);
        return Ok(());
    }
    report.copied.push(label);
    Ok(())
}

fn exceeds(dir: &Path, max_files: usize, worktree_id: FileId) -> Result<bool, String> {
    let mut count = 0;
    count_files(dir, max_files, worktree_id, &mut count)?;
    Ok(count > max_files)
}

/// Why not walk the whole tree: stopping once the count passes `limit` keeps
/// rejecting a huge tree as cheap as the limit itself. Subtrees that `copy_dir`
/// would skip as already present are still counted, so this may overcount.
fn count_files(
    dir: &Path,
    limit: usize,
    worktree_id: FileId,
    count: &mut usize,
) -> Result<(), String> {
    if is_worktree(dir, worktree_id) {
        return Ok(());
    }
    let children = match list_children(dir) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        listed => listed.map_err(|e| format!("{}: {e}", dir.display()))?,
    };
    for (name, file_type) in children {
        if *count > limit {
            return Ok(());
        }
        let child = dir.join(name);
        if !file_type.is_dir() {
            *count += 1;
        } else if !is_repository(&child) {
            count_files(&child, limit, worktree_id, count)?;
        }
    }
    Ok(())
}

/// Why not skip only the child whose `file_type` failed: it only stats on a
/// filesystem without `d_type`, and re-applying fills in what was skipped.
fn list_children(dir: &Path) -> io::Result<Vec<(OsString, FileType)>> {
    std::fs::read_dir(dir)?
        .map(|child| child.and_then(|c| Ok((c.file_name(), c.file_type()?))))
        .collect()
}

/// Why not propagate the error: a directory that cannot be stat-ed is not the
/// worktree, and reading it right after reports the failure anyway.
fn is_worktree(path: &Path, worktree_id: FileId) -> bool {
    std::fs::metadata(path).is_ok_and(|m| (m.dev(), m.ino()) == worktree_id)
}

/// Why not leave it to `create_dir_all`: it follows a symlinked ancestor, so
/// the entry would be seeded wherever that worktree link points.
fn has_symlinked_ancestor(worktree: &Path, relative: &Path) -> bool {
    relative
        .parent()
        .into_iter()
        .flat_map(Path::ancestors)
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .any(|ancestor| {
            worktree
                .join(ancestor)
                .symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink())
        })
}

/// Why not copy it: a nested worktree or submodule is its own checkout, and a
/// copied `.git` file would still point at the original's git directory.
fn is_repository(dir: &Path) -> bool {
    dir.join(".git").symlink_metadata().is_ok()
}

/// Runs each configured command in the new worktree with the paths exposed as
/// environment variables, so hooks stay repo-agnostic one-liners.
pub fn run_commands(
    shell: &str,
    target: &WorktreeTarget,
    event: &str,
    commands: &[String],
) -> Result<(), String> {
    for command in commands {
        let status = Command::new(shell)
            .arg("-c")
            .arg(command)
            .current_dir(&target.checkout_path)
            .env("MAIN", &target.repo_root)
            .env("WORKTREE", &target.checkout_path)
            .env("REPO", &target.repo_name)
            .env("BRANCH", target.branch.clone().unwrap_or_default())
            .env("EVENT", event)
            .status()
            .map_err(|e| format!("{shell} -c {command:?}: {e}"))?;

        if !status.success() {
            return Err(format!("{command:?} exited with {status}"));
        }
    }
    Ok(())
}

/// Only `worktree.created` carries the branch, and which of the concurrent
/// creation events wins the claim is a race — so `$BRANCH` is read back from
/// the checkout rather than left empty depending on the winner.
pub fn branch_of(checkout_path: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["-C", checkout_path, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    // A detached HEAD reports "HEAD", which is not a branch name.
    (!branch.is_empty() && branch != "HEAD").then_some(branch)
}

/// Why not accept any path: entries name files inside the checkout, and an
/// absolute or `..` entry would write outside the worktree herdr just created.
fn validate_entry(entry: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(entry);
    if entry.is_empty() {
        return Err("empty copy entry".to_string());
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => return Err(format!("{entry}: must be a relative path without `..`")),
        }
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_MAX_FILES;

    struct Pair {
        _root: tempfile::TempDir,
        main: PathBuf,
        worktree: PathBuf,
    }

    fn pair() -> Pair {
        let root = tempfile::tempdir().unwrap();
        let main = root.path().join("main");
        let worktree = root.path().join("worktree");
        std::fs::create_dir_all(&main).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        Pair {
            _root: root,
            main,
            worktree,
        }
    }

    fn entries(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_gitignored_file_is_copied_into_the_worktree() {
        let p = pair();
        std::fs::write(p.main.join(".env"), "TOKEN=main").unwrap();

        let report =
            copy_files(&p.main, &p.worktree, &entries(&[".env"]), DEFAULT_MAX_FILES).unwrap();

        assert_eq!(report.copied, vec![".env"]);
        assert_eq!(
            std::fs::read_to_string(p.worktree.join(".env")).unwrap(),
            "TOKEN=main"
        );
    }

    #[test]
    fn an_existing_destination_is_never_overwritten() {
        let p = pair();
        std::fs::write(p.main.join(".env"), "TOKEN=main").unwrap();
        std::fs::write(p.worktree.join(".env"), "TOKEN=local").unwrap();

        let report =
            copy_files(&p.main, &p.worktree, &entries(&[".env"]), DEFAULT_MAX_FILES).unwrap();

        assert_eq!(report.skipped_existing, vec![".env"]);
        assert!(report.copied.is_empty());
        assert_eq!(
            std::fs::read_to_string(p.worktree.join(".env")).unwrap(),
            "TOKEN=local"
        );
    }

    #[test]
    fn a_missing_source_is_reported_without_failing_the_run() {
        let p = pair();
        let report = copy_files(
            &p.main,
            &p.worktree,
            &entries(&[".env.local"]),
            DEFAULT_MAX_FILES,
        )
        .unwrap();
        assert_eq!(report.missing_source, vec![".env.local"]);
    }

    #[test]
    fn a_nested_entry_creates_its_parent_directories() {
        let p = pair();
        std::fs::create_dir_all(p.main.join("config")).unwrap();
        std::fs::write(p.main.join("config/secrets.yml"), "key: value").unwrap();

        copy_files(
            &p.main,
            &p.worktree,
            &entries(&["config/secrets.yml"]),
            DEFAULT_MAX_FILES,
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(p.worktree.join("config/secrets.yml")).unwrap(),
            "key: value"
        );
    }

    #[test]
    fn the_destination_is_a_real_file_not_a_symlink() {
        let p = pair();
        std::fs::write(p.main.join(".env"), "TOKEN=main").unwrap();

        copy_files(&p.main, &p.worktree, &entries(&[".env"]), DEFAULT_MAX_FILES).unwrap();

        let metadata = std::fs::symlink_metadata(p.worktree.join(".env")).unwrap();
        assert!(metadata.file_type().is_file());
        assert!(!metadata.file_type().is_symlink());
    }

    #[test]
    fn a_dangling_symlink_at_the_destination_counts_as_existing() {
        let p = pair();
        std::fs::write(p.main.join(".env"), "TOKEN=main").unwrap();
        std::os::unix::fs::symlink("/nonexistent", p.worktree.join(".env")).unwrap();

        let report =
            copy_files(&p.main, &p.worktree, &entries(&[".env"]), DEFAULT_MAX_FILES).unwrap();

        assert_eq!(report.skipped_existing, vec![".env"]);
    }

    #[test]
    fn an_entry_escaping_the_worktree_is_rejected() {
        let p = pair();
        assert!(
            copy_files(
                &p.main,
                &p.worktree,
                &entries(&["../outside"]),
                DEFAULT_MAX_FILES
            )
            .is_err()
        );
        assert!(
            copy_files(
                &p.main,
                &p.worktree,
                &entries(&["/etc/passwd"]),
                DEFAULT_MAX_FILES
            )
            .is_err()
        );
    }

    #[test]
    fn a_directory_is_merged_without_overwriting_tracked_files() {
        let p = pair();
        std::fs::create_dir_all(p.main.join("config/nested")).unwrap();
        std::fs::write(p.main.join("config/app.yml"), "main").unwrap();
        std::fs::write(p.main.join("config/nested/secrets.yml"), "secret").unwrap();
        std::fs::create_dir_all(p.worktree.join("config")).unwrap();
        std::fs::write(p.worktree.join("config/app.yml"), "tracked").unwrap();

        let report = copy_files(
            &p.main,
            &p.worktree,
            &entries(&["config"]),
            DEFAULT_MAX_FILES,
        )
        .unwrap();

        assert_eq!(report.copied, vec!["config/nested/secrets.yml"]);
        assert_eq!(report.skipped_existing, vec!["config/app.yml"]);
        assert_eq!(
            std::fs::read_to_string(p.worktree.join("config/app.yml")).unwrap(),
            "tracked"
        );
        assert_eq!(
            std::fs::read_to_string(p.worktree.join("config/nested/secrets.yml")).unwrap(),
            "secret"
        );
    }

    #[test]
    fn a_symlink_inside_a_directory_is_recreated_as_a_symlink() {
        let p = pair();
        std::fs::create_dir_all(p.main.join(".venv/bin")).unwrap();
        std::os::unix::fs::symlink("/usr/bin/python3", p.main.join(".venv/bin/python")).unwrap();

        copy_files(
            &p.main,
            &p.worktree,
            &entries(&[".venv"]),
            DEFAULT_MAX_FILES,
        )
        .unwrap();

        assert_eq!(
            std::fs::read_link(p.worktree.join(".venv/bin/python")).unwrap(),
            PathBuf::from("/usr/bin/python3")
        );
    }

    #[test]
    fn a_symlink_cycle_inside_a_directory_is_not_followed() {
        let p = pair();
        std::fs::create_dir_all(p.main.join("cache")).unwrap();
        std::os::unix::fs::symlink("..", p.main.join("cache/loop")).unwrap();

        let report = copy_files(
            &p.main,
            &p.worktree,
            &entries(&["cache"]),
            DEFAULT_MAX_FILES,
        )
        .unwrap();

        assert_eq!(report.copied, vec!["cache/loop"]);
        assert!(
            std::fs::symlink_metadata(p.worktree.join("cache/loop"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn a_symlinked_entry_is_copied_as_its_target() {
        let p = pair();
        std::fs::write(p.main.join(".env.shared"), "TOKEN=shared").unwrap();
        std::os::unix::fs::symlink(".env.shared", p.main.join(".env")).unwrap();

        copy_files(&p.main, &p.worktree, &entries(&[".env"]), DEFAULT_MAX_FILES).unwrap();

        let metadata = std::fs::symlink_metadata(p.worktree.join(".env")).unwrap();
        assert!(metadata.file_type().is_file());
        assert_eq!(
            std::fs::read_to_string(p.worktree.join(".env")).unwrap(),
            "TOKEN=shared"
        );
    }

    #[test]
    fn a_symlink_in_the_worktree_is_never_merged_through() {
        let p = pair();
        let outside = p._root.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(p.main.join("config")).unwrap();
        std::fs::write(p.main.join("config/secrets.yml"), "secret").unwrap();
        std::os::unix::fs::symlink(&outside, p.worktree.join("config")).unwrap();

        let report = copy_files(
            &p.main,
            &p.worktree,
            &entries(&["config"]),
            DEFAULT_MAX_FILES,
        )
        .unwrap();

        assert_eq!(report.skipped_existing, vec!["config"]);
        assert!(!outside.join("secrets.yml").exists());
    }

    #[test]
    fn an_entry_under_a_symlinked_worktree_directory_is_not_copied_through_it() {
        let p = pair();
        let outside = p._root.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(p.main.join("config/nested")).unwrap();
        std::fs::write(p.main.join("config/nested/secrets.yml"), "secret").unwrap();
        std::os::unix::fs::symlink(&outside, p.worktree.join("config")).unwrap();

        let report = copy_files(
            &p.main,
            &p.worktree,
            &entries(&["config/nested"]),
            DEFAULT_MAX_FILES,
        )
        .unwrap();

        assert_eq!(report.skipped_existing, vec!["config/nested"]);
        assert!(!outside.join("nested").exists());
    }

    #[test]
    fn a_created_directory_keeps_the_source_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let p = pair();
        std::fs::create_dir_all(p.main.join("secrets/readonly")).unwrap();
        std::fs::write(p.main.join("secrets/readonly/key"), "key").unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let set_mode = |path: &Path, mode: u32| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap()
        };
        set_mode(&p.main.join("secrets"), 0o700);
        set_mode(&p.main.join("secrets/readonly"), 0o500);

        let report = copy_files(
            &p.main,
            &p.worktree,
            &entries(&["secrets"]),
            DEFAULT_MAX_FILES,
        );
        let copied_mode = |rel: &str| mode(&p.worktree.join(rel));
        let modes = (copied_mode("secrets"), copied_mode("secrets/readonly"));
        set_mode(&p.main.join("secrets/readonly"), 0o700);
        set_mode(&p.worktree.join("secrets/readonly"), 0o700);

        assert_eq!(report.unwrap().copied, vec!["secrets/readonly/key"]);
        assert_eq!(modes, (0o700, 0o500));
    }

    #[test]
    fn a_worktree_nested_inside_main_is_not_copied_into_itself() {
        let root = tempfile::tempdir().unwrap();
        let main = root.path().to_path_buf();
        let worktree = main.join(".worktrees/feat");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(main.join(".env"), "TOKEN=main").unwrap();

        copy_files(&main, &worktree, &entries(&["."]), DEFAULT_MAX_FILES).unwrap();

        assert!(worktree.join(".env").exists());
        assert!(!worktree.join(".worktrees/feat").exists());
    }

    #[test]
    fn a_directory_over_the_file_limit_is_skipped_without_creating_anything() {
        let p = pair();
        std::fs::create_dir_all(p.main.join("node_modules/pkg")).unwrap();
        for name in ["a.js", "b.js", "pkg/c.js"] {
            std::fs::write(p.main.join("node_modules").join(name), "").unwrap();
        }
        std::fs::write(p.main.join(".env"), "TOKEN=main").unwrap();

        let report =
            copy_files(&p.main, &p.worktree, &entries(&["node_modules", ".env"]), 2).unwrap();

        assert_eq!(report.skipped_too_large, vec!["node_modules"]);
        assert_eq!(report.copied, vec![".env"]);
        assert!(!p.worktree.join("node_modules").exists());
    }

    #[test]
    fn a_directory_exactly_at_the_file_limit_is_copied() {
        let p = pair();
        std::fs::create_dir_all(p.main.join("config")).unwrap();
        std::fs::write(p.main.join("config/a.yml"), "").unwrap();
        std::fs::write(p.main.join("config/b.yml"), "").unwrap();

        let report = copy_files(&p.main, &p.worktree, &entries(&["config"]), 2).unwrap();

        assert!(report.skipped_too_large.is_empty());
        assert_eq!(report.copied.len(), 2);
    }

    #[test]
    fn a_nested_repository_inside_a_directory_is_skipped_and_reported() {
        let p = pair();
        std::fs::create_dir_all(p.main.join(".worktrees/other")).unwrap();
        std::fs::write(p.main.join(".worktrees/other/.git"), "gitdir: x").unwrap();
        std::fs::write(p.main.join(".worktrees/notes.md"), "").unwrap();

        let report = copy_files(
            &p.main,
            &p.worktree,
            &entries(&[".worktrees"]),
            DEFAULT_MAX_FILES,
        )
        .unwrap();

        assert_eq!(report.skipped_repositories, vec![".worktrees/other"]);
        assert_eq!(report.copied, vec![".worktrees/notes.md"]);
        assert!(!p.worktree.join(".worktrees/other").exists());
    }

    #[test]
    fn an_entry_that_is_itself_a_repository_is_still_copied() {
        let p = pair();
        std::fs::create_dir_all(p.main.join(".git")).unwrap();
        std::fs::write(p.main.join(".env"), "TOKEN=main").unwrap();

        copy_files(&p.main, &p.worktree, &entries(&["."]), DEFAULT_MAX_FILES).unwrap();

        assert!(p.worktree.join(".env").exists());
    }

    #[test]
    fn a_file_that_vanished_after_listing_is_reported_as_missing() {
        let p = pair();
        std::fs::write(p.main.join("present"), "").unwrap();
        let file_type = std::fs::symlink_metadata(p.main.join("present"))
            .unwrap()
            .file_type();
        let mut report = CopyReport::default();

        seed(
            &p.main.join("gone.tmp"),
            &p.worktree.join("gone.tmp"),
            Path::new("build/gone.tmp"),
            file_type,
            (0, 0),
            &mut report,
        )
        .unwrap();

        assert_eq!(report.missing_source, vec!["build/gone.tmp"]);
        assert!(report.copied.is_empty());
    }

    #[test]
    fn a_special_file_is_skipped_and_reported() {
        let p = pair();
        std::fs::create_dir_all(p.main.join("run")).unwrap();
        let fifo = p.main.join("run/pipe");
        let status = Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(status.success());

        let report =
            copy_files(&p.main, &p.worktree, &entries(&["run"]), DEFAULT_MAX_FILES).unwrap();

        assert_eq!(report.skipped_special, vec!["run/pipe"]);
        assert!(!p.worktree.join("run/pipe").exists());
    }

    /// Restores the mode on drop so the tempdir can still be cleaned up.
    struct Locked(PathBuf);

    impl Locked {
        fn new(path: PathBuf, mode: u32) -> Self {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            Self(path)
        }
    }

    impl Drop for Locked {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }

    #[test]
    fn a_flat_directory_over_the_file_limit_is_skipped() {
        let p = pair();
        std::fs::create_dir_all(p.main.join("dist")).unwrap();
        for name in ["a.js", "b.js", "c.js"] {
            std::fs::write(p.main.join("dist").join(name), "").unwrap();
        }

        let report = copy_files(&p.main, &p.worktree, &entries(&["dist"]), 1).unwrap();

        assert_eq!(report.skipped_too_large, vec!["dist"]);
    }

    #[test]
    fn a_missing_worktree_is_an_error() {
        let p = pair();
        let missing = p.worktree.join("gone");
        assert!(copy_files(&p.main, &missing, &entries(&[".env"]), DEFAULT_MAX_FILES).is_err());
    }

    #[test]
    fn a_file_that_cannot_be_written_fails_the_run() {
        let p = pair();
        std::fs::write(p.main.join(".env"), "TOKEN=main").unwrap();
        let _locked = Locked::new(p.worktree.clone(), 0o555);

        assert!(copy_files(&p.main, &p.worktree, &entries(&[".env"]), DEFAULT_MAX_FILES).is_err());
    }

    #[test]
    fn a_parent_directory_that_cannot_be_created_fails_the_run() {
        let p = pair();
        std::fs::create_dir_all(p.main.join("config")).unwrap();
        std::fs::write(p.main.join("config/secrets.yml"), "").unwrap();
        let _locked = Locked::new(p.worktree.clone(), 0o555);

        let result = copy_files(
            &p.main,
            &p.worktree,
            &entries(&["config/secrets.yml"]),
            DEFAULT_MAX_FILES,
        );

        assert!(result.is_err());
    }

    #[test]
    fn a_nested_directory_that_cannot_be_created_fails_the_run() {
        let p = pair();
        std::fs::create_dir_all(p.main.join("config/nested")).unwrap();
        std::fs::write(p.main.join("config/nested/secrets.yml"), "").unwrap();
        std::fs::create_dir_all(p.worktree.join("config")).unwrap();
        let _locked = Locked::new(p.worktree.join("config"), 0o555);

        assert!(
            copy_files(
                &p.main,
                &p.worktree,
                &entries(&["config"]),
                DEFAULT_MAX_FILES
            )
            .is_err()
        );
    }

    #[test]
    fn a_file_inside_a_directory_that_cannot_be_written_fails_the_run() {
        let p = pair();
        std::fs::create_dir_all(p.main.join("config")).unwrap();
        std::fs::write(p.main.join("config/secrets.yml"), "").unwrap();
        std::fs::create_dir_all(p.worktree.join("config")).unwrap();
        let _locked = Locked::new(p.worktree.join("config"), 0o555);

        assert!(
            copy_files(
                &p.main,
                &p.worktree,
                &entries(&["config"]),
                DEFAULT_MAX_FILES
            )
            .is_err()
        );
    }

    #[test]
    fn an_unreadable_source_directory_fails_the_run() {
        let p = pair();
        std::fs::create_dir_all(p.main.join("config/locked")).unwrap();
        let _locked = Locked::new(p.main.join("config/locked"), 0o000);

        assert!(
            copy_files(
                &p.main,
                &p.worktree,
                &entries(&["config"]),
                DEFAULT_MAX_FILES
            )
            .is_err()
        );
        assert!(!p.worktree.join("config").exists());
    }

    #[test]
    fn a_directory_that_vanished_after_listing_is_reported_as_missing() {
        let p = pair();
        let file_type = std::fs::metadata(&p.main).unwrap().file_type();
        let mut report = CopyReport::default();

        seed(
            &p.main.join("gone"),
            &p.worktree.join("gone"),
            Path::new("build/gone"),
            file_type,
            (0, 0),
            &mut report,
        )
        .unwrap();

        assert_eq!(report.missing_source, vec!["build/gone"]);
    }

    #[test]
    fn a_symlink_that_vanished_after_listing_is_reported_as_missing() {
        let p = pair();
        std::os::unix::fs::symlink("target", p.main.join("link")).unwrap();
        let file_type = std::fs::symlink_metadata(p.main.join("link"))
            .unwrap()
            .file_type();
        let mut report = CopyReport::default();

        seed(
            &p.main.join("gone"),
            &p.worktree.join("gone"),
            Path::new("gone"),
            file_type,
            (0, 0),
            &mut report,
        )
        .unwrap();

        assert_eq!(report.missing_source, vec!["gone"]);
    }

    #[test]
    fn a_directory_that_vanished_while_counting_counts_as_empty() {
        let p = pair();
        let mut count = 0;

        count_files(&p.main.join("gone"), DEFAULT_MAX_FILES, (0, 0), &mut count).unwrap();

        assert_eq!(count, 0);
    }

    #[test]
    fn a_symlink_that_cannot_be_created_fails_the_run() {
        let p = pair();
        std::fs::create_dir_all(p.main.join(".venv")).unwrap();
        std::os::unix::fs::symlink("/usr/bin/python3", p.main.join(".venv/python")).unwrap();
        std::fs::create_dir_all(p.worktree.join(".venv")).unwrap();
        let _locked = Locked::new(p.worktree.join(".venv"), 0o555);

        assert!(
            copy_files(
                &p.main,
                &p.worktree,
                &entries(&[".venv"]),
                DEFAULT_MAX_FILES
            )
            .is_err()
        );
    }

    #[test]
    fn commands_run_in_the_worktree_with_the_paths_exported() {
        let p = pair();
        let target = WorktreeTarget {
            repo_name: "my-app".to_string(),
            repo_root: p.main.to_string_lossy().to_string(),
            checkout_path: p.worktree.to_string_lossy().to_string(),
            is_linked_worktree: true,
            branch: Some("feat/x".to_string()),
        };

        run_commands(
            "/bin/sh",
            &target,
            "worktree.created",
            &entries(&["printf '%s %s %s' \"$REPO\" \"$BRANCH\" \"$EVENT\" > marker; pwd > cwd"]),
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(p.worktree.join("marker")).unwrap(),
            "my-app feat/x worktree.created"
        );
        assert!(
            std::fs::read_to_string(p.worktree.join("cwd"))
                .unwrap()
                .trim()
                .ends_with("worktree")
        );
    }

    #[test]
    fn the_branch_is_read_back_from_a_checkout_that_did_not_report_one() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap()
        };
        git(&["init", "--initial-branch=feat/from-git", "."]);
        git(&[
            "-c",
            "user.email=t@e.st",
            "-c",
            "user.name=t",
            "commit",
            "--allow-empty",
            "-m",
            "x",
        ]);

        assert_eq!(
            branch_of(&dir.path().to_string_lossy()),
            Some("feat/from-git".to_string())
        );
    }

    #[test]
    fn a_path_that_is_not_a_repository_reports_no_branch() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(branch_of(&dir.path().to_string_lossy()), None);
    }

    #[test]
    fn a_failing_command_stops_the_run() {
        let p = pair();
        let target = WorktreeTarget {
            repo_name: "my-app".to_string(),
            repo_root: p.main.to_string_lossy().to_string(),
            checkout_path: p.worktree.to_string_lossy().to_string(),
            is_linked_worktree: true,
            branch: None,
        };

        let result = run_commands(
            "/bin/sh",
            &target,
            "worktree.created",
            &entries(&["exit 3", "touch should-not-exist"]),
        );

        assert!(result.is_err());
        assert!(!p.worktree.join("should-not-exist").exists());
    }
}
