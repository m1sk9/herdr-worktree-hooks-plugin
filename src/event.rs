use serde_json::Value;

/// The worktree a hook run is about, as herdr describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeTarget {
    pub repo_name: String,
    pub repo_root: String,
    pub checkout_path: String,
    pub is_linked_worktree: bool,
    pub branch: Option<String>,
}

/// Why not index a fixed path like `data.workspace.worktree`: the payload shape
/// differs per event and the docs do not pin down whether the `{event, data}`
/// envelope is passed whole, so we look for the object that carries the fields.
pub fn find_target(payload: &Value) -> Option<WorktreeTarget> {
    find_targets(payload).into_iter().next()
}

/// Every worktree described anywhere in the value — used to seed claims from a
/// `workspace list` response, which carries one entry per open workspace.
pub fn find_targets(payload: &Value) -> Vec<WorktreeTarget> {
    let mut found = Vec::new();
    collect_objects(payload, &is_worktree_info, &mut found);
    found
        .into_iter()
        .filter_map(|info| {
            let checkout_path = info["checkout_path"].as_str()?.to_string();
            Some(WorktreeTarget {
                repo_name: info
                    .get("repo_name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                repo_root: info["repo_root"].as_str()?.to_string(),
                branch: find_branch(payload, &checkout_path),
                is_linked_worktree: info
                    .get("is_linked_worktree")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                checkout_path,
            })
        })
        .collect()
}

fn is_worktree_info(obj: &serde_json::Map<String, Value>) -> bool {
    obj.get("repo_root").and_then(Value::as_str).is_some()
        && obj.get("checkout_path").and_then(Value::as_str).is_some()
}

pub fn find_workspace_id(payload: &Value) -> Option<String> {
    find_object(payload, &|obj| {
        obj.get("workspace_id").and_then(Value::as_str).is_some()
    })
    .map(|obj| obj["workspace_id"].as_str().unwrap_or_default().to_string())
}

/// A payload that names only a workspace — and the `apply` action, which starts
/// from nothing but the focused workspace id — needs the paths fetched back from
/// herdr before anything else can be decided.
pub fn resolve_target<F>(
    payload: Option<&Value>,
    workspace_id_hint: Option<&str>,
    fetch_workspace: F,
) -> Result<Option<WorktreeTarget>, String>
where
    F: FnOnce(&str) -> Result<Value, String>,
{
    if let Some(target) = payload.and_then(find_target) {
        return Ok(Some(target));
    }

    let workspace_id = payload
        .and_then(find_workspace_id)
        .or_else(|| workspace_id_hint.map(str::to_string));
    let Some(workspace_id) = workspace_id else {
        return Ok(None);
    };

    Ok(find_target(&fetch_workspace(&workspace_id)?))
}

/// The branch lives on a sibling `WorktreeInfo` object keyed by `path`, so
/// prefer the one describing this checkout over any other in the payload.
fn find_branch(payload: &Value, checkout_path: &str) -> Option<String> {
    let matching = find_object(payload, &|obj| {
        obj.get("path").and_then(Value::as_str) == Some(checkout_path)
            && obj.get("branch").and_then(Value::as_str).is_some()
    });
    matching.map(|obj| obj["branch"].as_str().unwrap_or_default().to_string())
}

fn find_object<'a>(
    value: &'a Value,
    matches: &dyn Fn(&serde_json::Map<String, Value>) -> bool,
) -> Option<&'a serde_json::Map<String, Value>> {
    let mut found = Vec::new();
    collect_objects(value, matches, &mut found);
    found.into_iter().next()
}

fn collect_objects<'a>(
    value: &'a Value,
    matches: &dyn Fn(&serde_json::Map<String, Value>) -> bool,
    out: &mut Vec<&'a serde_json::Map<String, Value>>,
) {
    match value {
        Value::Object(obj) => {
            if matches(obj) {
                out.push(obj);
                return;
            }
            for child in obj.values() {
                collect_objects(child, matches, out);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_objects(child, matches, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn worktree_created() -> Value {
        json!({
            "event": "worktree_created",
            "data": {
                "type": "worktree_created",
                "workspace": {
                    "workspace_id": "w2S",
                    "label": "feat-sidebar-redesign",
                    "worktree": {
                        "repo_key": "/home/dev/src/github.com/acme/my-app/.git",
                        "repo_name": "my-app",
                        "repo_root": "/home/dev/src/github.com/acme/my-app",
                        "checkout_path": "/home/dev/src/worktrees/my-app/feat-sidebar-redesign",
                        "is_linked_worktree": true
                    }
                },
                "worktree": {
                    "path": "/home/dev/src/worktrees/my-app/feat-sidebar-redesign",
                    "branch": "feat/sidebar-redesign",
                    "is_bare": false,
                    "is_detached": false,
                    "is_linked_worktree": true,
                    "is_prunable": false,
                    "label": "my-app"
                }
            }
        })
    }

    #[test]
    fn worktree_created_payload_yields_both_checkout_and_main() {
        let target = find_target(&worktree_created()).expect("target");
        assert_eq!(
            target.checkout_path,
            "/home/dev/src/worktrees/my-app/feat-sidebar-redesign"
        );
        assert_eq!(target.repo_root, "/home/dev/src/github.com/acme/my-app");
        assert_eq!(target.repo_name, "my-app");
        assert_eq!(target.branch.as_deref(), Some("feat/sidebar-redesign"));
        assert!(target.is_linked_worktree);
    }

    #[test]
    fn workspace_created_payload_without_branch_still_resolves_paths() {
        let payload = json!({
            "data": {
                "type": "workspace_created",
                "workspace": {
                    "workspace_id": "w2S",
                    "worktree": {
                        "repo_name": "my-app",
                        "repo_root": "/home/dev/src/github.com/acme/my-app",
                        "checkout_path": "/home/dev/src/worktrees/my-app/feat-x",
                        "is_linked_worktree": true
                    }
                }
            }
        });
        let target = find_target(&payload).expect("target");
        assert_eq!(
            target.checkout_path,
            "/home/dev/src/worktrees/my-app/feat-x"
        );
        assert_eq!(target.branch, None);
    }

    #[test]
    fn main_checkout_is_reported_as_not_linked() {
        let payload = json!({
            "workspace": {
                "worktree": {
                    "repo_name": "my-app",
                    "repo_root": "/home/dev/src/github.com/acme/my-app",
                    "checkout_path": "/home/dev/src/github.com/acme/my-app",
                    "is_linked_worktree": false
                }
            }
        });
        assert!(!find_target(&payload).expect("target").is_linked_worktree);
    }

    #[test]
    fn focused_payload_carries_no_paths_and_requires_a_fetch() {
        let payload = json!({"data": {"type": "workspace_focused", "workspace_id": "w2S"}});
        assert!(find_target(&payload).is_none());
        assert_eq!(find_workspace_id(&payload).as_deref(), Some("w2S"));
    }

    #[test]
    fn focused_payload_resolves_through_the_workspace_lookup() {
        let payload = json!({"data": {"type": "workspace_focused", "workspace_id": "w2S"}});
        let target = resolve_target(Some(&payload), None, |id| {
            assert_eq!(id, "w2S");
            Ok(json!({
                "id": "cli:workspace:get",
                "result": {
                    "type": "workspace",
                    "workspace": {
                        "workspace_id": "w2S",
                        "worktree": {
                            "repo_name": "my-app",
                            "repo_root": "/home/dev/src/github.com/acme/my-app",
                            "checkout_path": "/home/dev/src/worktrees/my-app/feat-x",
                            "is_linked_worktree": true
                        }
                    }
                }
            }))
        })
        .expect("no error")
        .expect("target");
        assert_eq!(
            target.checkout_path,
            "/home/dev/src/worktrees/my-app/feat-x"
        );
    }

    #[test]
    fn payload_with_paths_never_triggers_a_lookup() {
        let target = resolve_target(Some(&worktree_created()), None, |_| {
            panic!("must not query herdr when the payload already has the paths")
        })
        .expect("no error")
        .expect("target");
        assert_eq!(target.repo_name, "my-app");
    }

    #[test]
    fn missing_payload_falls_back_to_the_workspace_id_from_the_environment() {
        let target = resolve_target(None, Some("w2Q"), |id| {
            assert_eq!(id, "w2Q");
            Ok(json!({
                "worktree": {
                    "repo_name": "my-app",
                    "repo_root": "/home/dev/src/github.com/acme/my-app",
                    "checkout_path": "/home/dev/src/worktrees/my-app/feat-y",
                    "is_linked_worktree": true
                }
            }))
        })
        .expect("no error")
        .expect("target");
        assert_eq!(
            target.checkout_path,
            "/home/dev/src/worktrees/my-app/feat-y"
        );
    }

    #[test]
    fn a_workspace_without_a_worktree_resolves_to_nothing() {
        let payload = json!({"data": {"workspace_id": "w2M"}});
        let resolved = resolve_target(Some(&payload), None, |_| {
            Ok(json!({"result": {"workspace": {"workspace_id": "w2M", "label": "chezmoi"}}}))
        })
        .expect("no error");
        assert_eq!(resolved, None);
    }

    #[test]
    fn a_workspace_list_yields_every_worktree_it_contains() {
        let payload = json!({
            "result": {
                "workspaces": [
                    {"workspace_id": "w2C", "worktree": {
                        "repo_name": "my-app",
                        "repo_root": "/home/dev/src/github.com/acme/my-app",
                        "checkout_path": "/home/dev/src/github.com/acme/my-app",
                        "is_linked_worktree": false}},
                    {"workspace_id": "w2M"},
                    {"workspace_id": "w2Q", "worktree": {
                        "repo_name": "my-app",
                        "repo_root": "/home/dev/src/github.com/acme/my-app",
                        "checkout_path": "/home/dev/src/worktrees/my-app/feat-a",
                        "is_linked_worktree": true}},
                    {"workspace_id": "w2R", "worktree": {
                        "repo_name": "my-app",
                        "repo_root": "/home/dev/src/github.com/acme/my-app",
                        "checkout_path": "/home/dev/src/worktrees/my-app/feat-b",
                        "is_linked_worktree": true}}
                ]
            }
        });

        let targets = find_targets(&payload);

        assert_eq!(targets.len(), 3);
        let linked: Vec<_> = targets
            .iter()
            .filter(|t| t.is_linked_worktree)
            .map(|t| t.checkout_path.as_str())
            .collect();
        assert_eq!(
            linked,
            vec![
                "/home/dev/src/worktrees/my-app/feat-a",
                "/home/dev/src/worktrees/my-app/feat-b"
            ]
        );
    }

    #[test]
    fn a_payload_with_nothing_identifying_resolves_to_nothing() {
        let resolved = resolve_target(
            Some(&json!({"data": {"type": "layout_updated"}})),
            None,
            |_| panic!("nothing to look up"),
        )
        .expect("no error");
        assert_eq!(resolved, None);
    }
}
