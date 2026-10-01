use std::fs::FileType;
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
    pub missing_source: Vec<String>,
}

/// Copies each entry from the main checkout into the worktree, merging
/// directories file by file. Anything that already exists in the worktree is
/// left untouched — the whole point is to seed a fresh checkout, not to
/// overwrite local edits.
///
/// A directory holding more than `max_files` files is skipped as a whole
/// rather than failing the run: a failure releases the claim, so every later
/// event would hit the same limit again and `run` would never get to rebuild it.
pub fn copy_files(
    main: &Path,
    worktree: &Path,
    entries: &[String],
    max_files: usize,
) -> Result<CopyReport, String> {
    let mut report = CopyReport::default();
    let worktree_id = file_id(worktree)?;

    for entry in entries {
        let relative = validate_entry(entry)?;
        let source = main.join(&relative);
        let destination = worktree.join(&relative);

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
        if metadata.is_dir() {
            copy_dir(
                &source,
                &destination,
                Path::new(entry),
                worktree_id,
                &mut report,
            )?;
        } else {
            copy_leaf(
                &source,
                &destination,
                Path::new(entry),
                metadata.file_type(),
                &mut report,
            )?;
        }
    }

    Ok(report)
}

fn copy_dir(
    source: &Path,
    destination: &Path,
    label: &Path,
    worktree_id: (u64, u64),
    report: &mut CopyReport,
) -> Result<(), String> {
    // Why not descend: a worktree nested inside the main checkout would be
    // copied into itself, growing while it is being walked.
    if file_id(source)? == worktree_id {
        return Ok(());
    }

    match destination.symlink_metadata() {
        Ok(existing) if existing.is_dir() => {}
        // Why not merge through it: a file or symlink at this path belongs to
        // the worktree, and following a symlink could write outside of it.
        Ok(_) => {
            report
                .skipped_existing
                .push(label.to_string_lossy().into_owned());
            return Ok(());
        }
        Err(_) => std::fs::create_dir(destination)
            .map_err(|e| format!("{}: {e}", destination.display()))?,
    }

    let children = std::fs::read_dir(source).map_err(|e| format!("{}: {e}", source.display()))?;
    for child in children {
        let child = child.map_err(|e| format!("{}: {e}", source.display()))?;
        let file_type = child
            .file_type()
            .map_err(|e| format!("{}: {e}", child.path().display()))?;
        let from = child.path();
        let to = destination.join(child.file_name());
        let label = label.join(child.file_name());

        if file_type.is_dir() {
            copy_dir(&from, &to, &label, worktree_id, report)?;
        } else {
            copy_leaf(&from, &to, &label, file_type, report)?;
        }
    }
    Ok(())
}

fn copy_leaf(
    source: &Path,
    destination: &Path,
    label: &Path,
    file_type: FileType,
    report: &mut CopyReport,
) -> Result<(), String> {
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
            std::fs::read_link(source).map_err(|e| format!("{}: {e}", source.display()))?;
        symlink(&target, destination)
            .map_err(|e| format!("{} -> {}: {e}", destination.display(), target.display()))?;
    } else if file_type.is_file() {
        std::fs::copy(source, destination)
            .map_err(|e| format!("{} -> {}: {e}", source.display(), destination.display()))?;
    } else {
        report.skipped_special.push(label);
        return Ok(());
    }
    report.copied.push(label);
    Ok(())
}

fn exceeds(dir: &Path, max_files: usize, worktree_id: (u64, u64)) -> Result<bool, String> {
    let mut count = 0;
    count_files(dir, max_files, worktree_id, &mut count)?;
    Ok(count > max_files)
}

/// Walks exactly what `copy_dir` would copy, stopping as soon as the count
/// passes `limit` so a huge tree costs no more than the limit to reject.
fn count_files(
    dir: &Path,
    limit: usize,
    worktree_id: (u64, u64),
    count: &mut usize,
) -> Result<(), String> {
    if file_id(dir)? == worktree_id {
        return Ok(());
    }
    let children = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for child in children {
        if *count > limit {
            return Ok(());
        }
        let child = child.map_err(|e| format!("{}: {e}", dir.display()))?;
        let file_type = child
            .file_type()
            .map_err(|e| format!("{}: {e}", child.path().display()))?;
        if file_type.is_dir() {
            count_files(&child.path(), limit, worktree_id, count)?;
        } else {
            *count += 1;
        }
    }
    Ok(())
}

fn file_id(path: &Path) -> Result<(u64, u64), String> {
    let metadata = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((metadata.dev(), metadata.ino()))
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
