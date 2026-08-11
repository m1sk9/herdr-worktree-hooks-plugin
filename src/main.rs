mod actions;
mod claim;
mod config;
mod event;
mod herdr;

use std::path::{Path, PathBuf};
use std::time::Duration;

use claim::ClaimStore;
use config::Config;
use event::WorktreeTarget;

/// The event that unambiguously means "this worktree did not exist a moment
/// ago". Every other subscribed event needs the freshness guard.
const CREATED_EVENT: &str = "worktree.created";

macro_rules! log {
    ($($arg:tt)*) => { println!("[worktree-hooks] {}", format!($($arg)*)) };
}

fn main() {
    if let Err(message) = run() {
        eprintln!("[worktree-hooks] error: {message}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("on-event") => on_event(),
        Some("seed") => seed(),
        Some("apply") => apply(args.iter().any(|a| a == "--force")),
        Some(other) => Err(format!("unknown subcommand {other:?}")),
    }
}

/// Fires for every subscribed event. Logs what it saw before deciding anything,
/// so `herdr plugin log list` doubles as an event probe.
fn on_event() -> Result<(), String> {
    let event_name = env_string("HERDR_PLUGIN_EVENT").unwrap_or_else(|| "unknown".to_string());
    let payload = parse_payload();

    let target = event::resolve_target(
        payload.as_ref(),
        env_string("HERDR_WORKSPACE_ID").as_deref(),
        herdr::workspace_get,
    )?;

    let Some(target) = target else {
        log!("event={event_name} no worktree in scope");
        return Ok(());
    };
    log!(
        "event={event_name} checkout={} repo_root={} event_branch={} linked={}",
        target.checkout_path,
        target.repo_root,
        target.branch.as_deref().unwrap_or("-"),
        target.is_linked_worktree
    );

    if !target.is_linked_worktree {
        log!("skip: main checkout, not a linked worktree");
        return Ok(());
    }

    let config = Config::load(env_path("HERDR_PLUGIN_CONFIG_DIR").as_deref())?;
    let store = ClaimStore::new(&require_state_dir()?).map_err(|e| e.to_string())?;

    if event_name != CREATED_EVENT && !is_fresh(&target.checkout_path, config.fresh_window_secs()) {
        log!(
            "skip: {event_name} on a checkout older than {}s",
            config.fresh_window_secs()
        );
        return Ok(());
    }
    if !store
        .try_acquire(&target.checkout_path)
        .map_err(|e| e.to_string())?
    {
        log!("skip: already applied to this worktree");
        return Ok(());
    }

    apply_to(&config, &target, &event_name).inspect_err(|_| {
        let _ = store.release(&target.checkout_path);
        log!("released the claim so the next event retries");
    })
}

/// Runs once per session before any worktree can be created, marking everything
/// that already exists as done — otherwise `workspace.focused` on a restored
/// worktree would look exactly like a brand new one.
fn seed() -> Result<(), String> {
    let store = ClaimStore::new(&require_state_dir()?).map_err(|e| e.to_string())?;
    let targets = event::find_targets(&herdr::workspace_list()?);

    let mut seeded = 0;
    for target in targets.iter().filter(|t| t.is_linked_worktree) {
        if store
            .try_acquire(&target.checkout_path)
            .map_err(|e| e.to_string())?
        {
            seeded += 1;
        }
    }
    log!(
        "seeded {seeded} existing worktree(s) as already applied ({} open)",
        targets.len()
    );
    Ok(())
}

/// Manual escape hatch for the workspace action: re-apply to the focused
/// worktree even though its claim was already taken.
fn apply(force: bool) -> Result<(), String> {
    let workspace_id = env_string("HERDR_WORKSPACE_ID")
        .ok_or("HERDR_WORKSPACE_ID is not set; run this from a workspace action")?;

    let target = event::resolve_target(None, Some(&workspace_id), herdr::workspace_get)?
        .ok_or_else(|| format!("workspace {workspace_id} is not backed by a worktree"))?;
    if !target.is_linked_worktree {
        return Err("this workspace is the main checkout, not a linked worktree".to_string());
    }

    let config = Config::load(env_path("HERDR_PLUGIN_CONFIG_DIR").as_deref())?;
    let store = ClaimStore::new(&require_state_dir()?).map_err(|e| e.to_string())?;
    let acquired = store
        .try_acquire(&target.checkout_path)
        .map_err(|e| e.to_string())?;

    if !acquired && !force {
        log!("skip: already applied to this worktree (use --force to re-run)");
        return Ok(());
    }
    apply_to(&config, &target, "action.apply")
}

fn apply_to(config: &Config, target: &WorktreeTarget, event_name: &str) -> Result<(), String> {
    let resolved = config.resolve(&target.repo_name, &target.repo_root);
    let target = &WorktreeTarget {
        branch: target
            .branch
            .clone()
            .or_else(|| actions::branch_of(&target.checkout_path)),
        ..target.clone()
    };

    let report = actions::copy_files(
        Path::new(&target.repo_root),
        Path::new(&target.checkout_path),
        &resolved.copy,
    )?;
    log!(
        "copied={:?} skipped_existing={:?} missing_in_main={:?}",
        report.copied,
        report.skipped_existing,
        report.missing_source
    );

    if !resolved.run.is_empty() {
        log!("running {} command(s)", resolved.run.len());
        actions::run_commands(&config.shell(), target, event_name, &resolved.run)?;
    }
    Ok(())
}

/// The claim store is what makes repeated events harmless, so a missing state
/// directory is a hard error rather than a silent fall-through.
fn require_state_dir() -> Result<PathBuf, String> {
    env_path("HERDR_PLUGIN_STATE_DIR")
        .ok_or_else(|| "HERDR_PLUGIN_STATE_DIR is not set".to_string())
}

fn parse_payload() -> Option<serde_json::Value> {
    let raw = env_string("HERDR_PLUGIN_EVENT_JSON")?;
    match serde_json::from_str(&raw) {
        Ok(value) => Some(value),
        Err(e) => {
            log!("HERDR_PLUGIN_EVENT_JSON is not valid JSON ({e}); falling back to a lookup");
            None
        }
    }
}

fn is_fresh(checkout_path: &str, window_secs: u64) -> bool {
    let Ok(metadata) = std::fs::metadata(checkout_path) else {
        return false;
    };
    let Ok(created) = metadata.created().or_else(|_| metadata.modified()) else {
        return false;
    };
    match created.elapsed() {
        Ok(age) => age <= Duration::from_secs(window_secs),
        // A creation time in the future means clock skew, not an old checkout.
        Err(_) => true,
    }
}

fn env_string(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_just_created_checkout_is_fresh() {
        let dir = tempfile::tempdir().unwrap();
        assert!(is_fresh(&dir.path().to_string_lossy(), 300));
    }

    #[test]
    fn a_checkout_outside_the_window_is_not_fresh() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_fresh(&dir.path().to_string_lossy(), 0));
    }

    #[test]
    fn a_missing_checkout_is_not_fresh() {
        assert!(!is_fresh("/nonexistent/worktree", 300));
    }
}
