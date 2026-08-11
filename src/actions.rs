use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::event::WorktreeTarget;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CopyReport {
    pub copied: Vec<String>,
    pub skipped_existing: Vec<String>,
    pub missing_source: Vec<String>,
}

/// Copies (never links) each entry from the main checkout into the worktree.
/// An entry that already exists in the worktree is left untouched — the whole
/// point is to seed a fresh checkout, not to overwrite local edits.
pub fn copy_files(main: &Path, worktree: &Path, entries: &[String]) -> Result<CopyReport, String> {
    let mut report = CopyReport::default();

    for entry in entries {
        let relative = validate_entry(entry)?;
        let source = main.join(&relative);
        let destination = worktree.join(&relative);

        if !source.exists() {
            report.missing_source.push(entry.clone());
            continue;
        }
        if source.is_dir() {
            return Err(format!("{entry}: directories are not supported"));
        }
        if destination.symlink_metadata().is_ok() {
            report.skipped_existing.push(entry.clone());
            continue;
        }

        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        std::fs::copy(&source, &destination)
            .map_err(|e| format!("{} -> {}: {e}", source.display(), destination.display()))?;
        report.copied.push(entry.clone());
    }

    Ok(report)
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

        let report = copy_files(&p.main, &p.worktree, &entries(&[".env"])).unwrap();

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

        let report = copy_files(&p.main, &p.worktree, &entries(&[".env"])).unwrap();

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
        let report = copy_files(&p.main, &p.worktree, &entries(&[".env.local"])).unwrap();
        assert_eq!(report.missing_source, vec![".env.local"]);
    }

    #[test]
    fn a_nested_entry_creates_its_parent_directories() {
        let p = pair();
        std::fs::create_dir_all(p.main.join("config")).unwrap();
        std::fs::write(p.main.join("config/secrets.yml"), "key: value").unwrap();

        copy_files(&p.main, &p.worktree, &entries(&["config/secrets.yml"])).unwrap();

        assert_eq!(
            std::fs::read_to_string(p.worktree.join("config/secrets.yml")).unwrap(),
            "key: value"
        );
    }

    #[test]
    fn the_destination_is_a_real_file_not_a_symlink() {
        let p = pair();
        std::fs::write(p.main.join(".env"), "TOKEN=main").unwrap();

        copy_files(&p.main, &p.worktree, &entries(&[".env"])).unwrap();

        let metadata = std::fs::symlink_metadata(p.worktree.join(".env")).unwrap();
        assert!(metadata.file_type().is_file());
        assert!(!metadata.file_type().is_symlink());
    }

    #[test]
    fn a_dangling_symlink_at_the_destination_counts_as_existing() {
        let p = pair();
        std::fs::write(p.main.join(".env"), "TOKEN=main").unwrap();
        std::os::unix::fs::symlink("/nonexistent", p.worktree.join(".env")).unwrap();

        let report = copy_files(&p.main, &p.worktree, &entries(&[".env"])).unwrap();

        assert_eq!(report.skipped_existing, vec![".env"]);
    }

    #[test]
    fn an_entry_escaping_the_worktree_is_rejected() {
        let p = pair();
        assert!(copy_files(&p.main, &p.worktree, &entries(&["../outside"])).is_err());
        assert!(copy_files(&p.main, &p.worktree, &entries(&["/etc/passwd"])).is_err());
    }

    #[test]
    fn a_directory_entry_is_rejected() {
        let p = pair();
        std::fs::create_dir_all(p.main.join("config")).unwrap();
        assert!(copy_files(&p.main, &p.worktree, &entries(&["config"])).is_err());
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
