//! End-to-end tests against a real herdr server.
//!
//! Ignored by default because they need a herdr binary and start a server:
//!
//! ```text
//! cargo test --test e2e -- --ignored
//! ```
//!
//! `HERDR_E2E_BIN` selects the herdr binary (default: `herdr` on `PATH`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

const PLUGIN_ID: &str = "m1sk9.worktree-hooks";
const TIMEOUT: Duration = Duration::from_secs(30);

/// A herdr server confined to a throwaway `HOME`, so running the suite from
/// inside a live herdr session never touches that session.
struct Herdr {
    _base: TempDir,
    root: PathBuf,
    server: Option<Child>,
}

impl Herdr {
    fn new() -> Self {
        build_plugin();
        // Why not a deeper directory: the server's unix socket path must fit
        // in sun_path (104 bytes on macOS).
        let base = tempfile::Builder::new()
            .prefix("hde2e")
            .tempdir()
            .expect("create temp dir");
        // Why canonicalize: herdr reports a checkout reopened by path in its
        // canonical spelling (/private/var on macOS) but a created one as
        // given, and the claim key is path-sensitive.
        let root = fs::canonicalize(base.path()).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        Self {
            _base: base,
            root,
            server: None,
        }
    }

    fn start() -> Self {
        let mut herdr = Self::new();
        herdr.start_server();
        herdr
    }

    fn start_server(&mut self) {
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path("server.log"))
            .unwrap();
        let mut server = Command::new(herdr_bin());
        isolate(&mut server, &self.root);
        self.server = Some(
            server
                .arg("server")
                .stdin(Stdio::null())
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .expect("spawn herdr server"),
        );
        self.wait_until("the server is ready", || {
            self.command(&["workspace", "list"]).status.success()
        });
    }

    fn stop_server(&mut self) {
        let Some(mut server) = self.server.take() else {
            return;
        };
        let _ = self.command(&["server", "stop"]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = server.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = server.kill();
        let _ = server.wait();
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn command(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(herdr_bin());
        isolate(&mut cmd, &self.root);
        cmd.args(args).output().expect("run herdr")
    }

    fn run(&self, args: &[&str]) -> Value {
        let output = self.command(args);
        assert!(
            output.status.success(),
            "herdr {} failed: {}\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr),
            self.diagnostics()
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        serde_json::from_str(&stdout).unwrap_or(Value::String(stdout.into_owned()))
    }

    fn link_plugin(&self) {
        self.run(&["plugin", "link", env!("CARGO_MANIFEST_DIR")]);
    }

    fn write_config(&self, toml: &str) {
        let output = self.command(&["plugin", "config-dir", PLUGIN_ID]);
        assert!(output.status.success(), "herdr plugin config-dir failed");
        let dir = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.toml"), toml).unwrap();
    }

    /// Returns the id of the workspace herdr opened for the new checkout.
    fn create_worktree(&self, repo: &Path, branch: &str, path: &Path) -> String {
        let response = self.run(&[
            "worktree",
            "create",
            "--cwd",
            repo.to_str().unwrap(),
            "--branch",
            branch,
            "--path",
            path.to_str().unwrap(),
            "--no-focus",
            "--trust-repository",
        ]);
        response["result"]["workspace"]["workspace_id"]
            .as_str()
            .expect("worktree create returns a workspace id")
            .to_string()
    }

    fn remove_worktree(&self, workspace_id: &str) {
        self.run(&[
            "worktree",
            "remove",
            "--workspace",
            workspace_id,
            "--force",
            "--trust-repository",
        ]);
    }

    fn invoke_action(&self, workspace_id: &str, action: &str) {
        self.run(&["workspace", "focus", workspace_id]);
        self.run(&[
            "plugin",
            "action",
            "invoke",
            &format!("{PLUGIN_ID}.{action}"),
        ]);
    }

    /// Stdout of the plugin's finished command runs, oldest first.
    fn finished_runs(&self) -> Vec<String> {
        let response = self.run(&["plugin", "log", "list", "--plugin", PLUGIN_ID]);
        let mut runs: Vec<Value> = response["result"]["logs"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|run| run["finished_unix_ms"].is_number())
            .collect();
        runs.sort_by_key(|run| run["started_unix_ms"].as_u64());
        runs.iter()
            .map(|run| run["stdout"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    fn runs_mentioning(&self, needle: &str) -> Vec<String> {
        self.finished_runs()
            .into_iter()
            .filter(|stdout| stdout.contains(needle))
            .collect()
    }

    /// Waits for both creation events of `checkout` to be handled, so that
    /// asserting on what did *not* happen is not just asserting too early.
    fn wait_for_creation_events(&self, checkout: &Path) {
        let needle = checkout_marker(checkout);
        self.wait_until("both creation events are handled", || {
            self.runs_mentioning(&needle).len() >= 2
        });
    }

    fn wait_until(&self, what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting until {what}\n{}",
                self.diagnostics()
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Everything needed to tell a plugin failure from a herdr one, since a
    /// failing CI run is the only place some of these tests ever run.
    fn diagnostics(&self) -> String {
        let plugin_log = self.command(&["plugin", "log", "list", "--plugin", PLUGIN_ID]);
        format!(
            "--- plugin log ---\n{}{}\n--- server log ---\n{}",
            String::from_utf8_lossy(&plugin_log.stdout),
            String::from_utf8_lossy(&plugin_log.stderr),
            fs::read_to_string(self.path("server.log")).unwrap_or_default()
        )
    }
}

impl Drop for Herdr {
    fn drop(&mut self) {
        self.stop_server();
    }
}

fn herdr_bin() -> PathBuf {
    std::env::var_os("HERDR_E2E_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("herdr"))
}

/// Why strip every `HERDR_*` variable: inside a herdr pane they point at the
/// live session's socket and workspace, which the test must never reach.
fn isolate(cmd: &mut Command, base: &Path) {
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("HERDR_") {
            cmd.env_remove(&key);
        }
    }
    let home = base.join("home");
    cmd.env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("HERDR_SOCKET_PATH", base.join("herdr.sock"));
}

/// The linked plugin runs `target/release/...`, so a stale release build would
/// silently test old code.
fn build_plugin() {
    static BUILT: OnceLock<()> = OnceLock::new();
    BUILT.get_or_init(|| {
        let status = Command::new(env!("CARGO"))
            .args(["build", "--release", "--manifest-path"])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
            .status()
            .expect("run cargo build");
        assert!(status.success(), "cargo build --release failed");
    });
}

/// How the plugin's log line names a checkout; the trailing space keeps `wt`
/// from also matching `wt2`.
fn checkout_marker(checkout: &Path) -> String {
    format!("checkout={} ", checkout.display())
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=e2e",
            "-c",
            "user.email=e2e@example.invalid",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A main checkout whose `.env` is gitignored, plus any committed `tracked`
/// files.
fn init_repo(herdr: &Herdr, tracked: &[(&str, &str)]) -> PathBuf {
    let repo = herdr.path("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "--quiet", "--initial-branch=main"]);
    fs::write(repo.join(".gitignore"), "/.env\n").unwrap();
    for (name, content) in tracked {
        fs::write(repo.join(name), content).unwrap();
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "--quiet", "-m", "init"]);
    fs::write(repo.join(".env"), "SECRET=1\n").unwrap();
    repo
}

#[test]
#[ignore = "needs a herdr binary; run with --ignored"]
fn a_new_worktree_receives_the_ignored_env_file() {
    let herdr = Herdr::start();
    let repo = init_repo(&herdr, &[]);
    herdr.link_plugin();

    let worktree = herdr.path("wt");
    herdr.create_worktree(&repo, "feature", &worktree);

    herdr.wait_until(".env is copied", || worktree.join(".env").exists());
    assert_eq!(
        fs::read_to_string(worktree.join(".env")).unwrap(),
        "SECRET=1\n"
    );
}

#[test]
#[ignore = "needs a herdr binary; run with --ignored"]
fn both_creation_events_apply_the_hooks_only_once() {
    let herdr = Herdr::start();
    let repo = init_repo(&herdr, &[]);
    herdr.link_plugin();

    let worktree = herdr.path("wt");
    herdr.create_worktree(&repo, "feature", &worktree);
    herdr.wait_for_creation_events(&worktree);

    let applied = herdr
        .runs_mentioning(&checkout_marker(&worktree))
        .into_iter()
        .filter(|stdout| stdout.contains("copy took"))
        .count();
    assert_eq!(applied, 1, "{}", herdr.diagnostics());
}

#[test]
#[ignore = "needs a herdr binary; run with --ignored"]
fn run_commands_execute_in_the_worktree_with_its_variables() {
    let herdr = Herdr::start();
    let repo = init_repo(&herdr, &[]);
    herdr.write_config(
        r#"
shell = "/bin/sh"

[defaults]
run = ['printf "%s\n" "$REPO" "$BRANCH" "$EVENT" "$MAIN" "$WORKTREE" "$PWD" > run.out']
"#,
    );
    herdr.link_plugin();

    let worktree = herdr.path("wt");
    herdr.create_worktree(&repo, "feature", &worktree);

    let out = worktree.join("run.out");
    herdr.wait_until("the run command writes run.out", || {
        fs::read_to_string(&out).is_ok_and(|s| s.lines().count() == 6)
    });
    let out = fs::read_to_string(out).unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "repo");
    assert_eq!(lines[1], "feature");
    // Which of the two racing events wins the claim is not deterministic.
    assert!(
        ["worktree.created", "workspace.created"].contains(&lines[2]),
        "unexpected EVENT {:?}",
        lines[2]
    );
    assert_eq!(Path::new(lines[3]), repo);
    assert_eq!(Path::new(lines[4]), worktree);
    assert_eq!(
        fs::canonicalize(lines[5]).unwrap(),
        fs::canonicalize(&worktree).unwrap()
    );
}

#[test]
#[ignore = "needs a herdr binary; run with --ignored"]
fn a_file_already_in_the_worktree_is_not_overwritten() {
    let herdr = Herdr::start();
    let repo = init_repo(&herdr, &[("settings.txt", "committed\n")]);
    fs::write(repo.join("settings.txt"), "local edit\n").unwrap();
    herdr.write_config(
        r#"
[defaults]
copy = [".env", "settings.txt"]
"#,
    );
    herdr.link_plugin();

    let worktree = herdr.path("wt");
    herdr.create_worktree(&repo, "feature", &worktree);
    herdr.wait_for_creation_events(&worktree);

    assert!(worktree.join(".env").exists(), "{}", herdr.diagnostics());
    assert_eq!(
        fs::read_to_string(worktree.join("settings.txt")).unwrap(),
        "committed\n"
    );
}

#[test]
#[ignore = "needs a herdr binary; run with --ignored"]
fn worktrees_restored_at_startup_are_not_treated_as_new() {
    let mut herdr = Herdr::start();
    let repo = init_repo(&herdr, &[]);
    let worktree = herdr.path("wt");
    let workspace_id = herdr.create_worktree(&repo, "feature", &worktree);

    herdr.stop_server();
    herdr.link_plugin();
    herdr.start_server();
    herdr.wait_until("the startup hook seeds the worktree", || {
        !herdr.runs_mentioning("seeded 1 existing").is_empty()
    });

    // Reopening a checkout emits workspace.created, and this one is still
    // inside the freshness window, so only the seeded claim keeps it skipped.
    herdr.run(&["workspace", "close", &workspace_id]);
    herdr.run(&[
        "worktree",
        "open",
        "--cwd",
        repo.to_str().unwrap(),
        "--path",
        worktree.to_str().unwrap(),
        "--no-focus",
        "--trust-repository",
    ]);
    let needle = checkout_marker(&worktree);
    herdr.wait_until("the reopened workspace's event is handled", || {
        !herdr.runs_mentioning(&needle).is_empty()
    });

    assert!(
        herdr
            .runs_mentioning(&needle)
            .iter()
            .all(|stdout| stdout.contains("skip: already applied")),
        "{}",
        herdr.diagnostics()
    );
    assert!(!worktree.join(".env").exists());
}

#[test]
#[ignore = "needs a herdr binary; run with --ignored"]
fn a_worktree_recreated_at_the_same_path_is_applied_again() {
    let herdr = Herdr::start();
    let repo = init_repo(&herdr, &[]);
    herdr.link_plugin();

    let worktree = herdr.path("wt");
    let workspace_id = herdr.create_worktree(&repo, "feature", &worktree);
    herdr.wait_for_creation_events(&worktree);
    herdr.remove_worktree(&workspace_id);
    herdr.wait_until("the checkout is removed", || !worktree.exists());

    herdr.create_worktree(&repo, "feature", &worktree);

    herdr.wait_until(".env is copied into the new checkout", || {
        worktree.join(".env").exists()
    });
}

#[test]
#[ignore = "needs a herdr binary; run with --ignored"]
fn the_apply_action_reapplies_to_the_focused_worktree() {
    let herdr = Herdr::start();
    let repo = init_repo(&herdr, &[]);
    herdr.link_plugin();

    let worktree = herdr.path("wt");
    let workspace_id = herdr.create_worktree(&repo, "feature", &worktree);
    herdr.wait_for_creation_events(&worktree);
    fs::remove_file(worktree.join(".env")).unwrap();

    herdr.invoke_action(&workspace_id, "apply");

    herdr.wait_until(".env is copied again", || worktree.join(".env").exists());
}

#[test]
#[ignore = "needs a herdr binary; run with --ignored"]
fn cleanup_commands_run_in_the_main_checkout_after_removal() {
    let herdr = Herdr::start();
    let repo = init_repo(&herdr, &[]);
    let out = herdr.path("cleanup.out");
    herdr.write_config(&format!(
        r#"
shell = "/bin/sh"

[defaults]
cleanup = [
    'exit 3',
    'printf "%s\n" "$REPO" "$BRANCH" "$EVENT" "$MAIN" "$WORKTREE" "$PWD" > "{}"',
]
"#,
        out.display()
    ));
    herdr.link_plugin();

    let worktree = herdr.path("wt");
    let workspace_id = herdr.create_worktree(&repo, "feature", &worktree);
    herdr.wait_for_creation_events(&worktree);
    assert!(!out.exists(), "cleanup ran before removal");

    herdr.remove_worktree(&workspace_id);

    herdr.wait_until("the cleanup command writes cleanup.out", || {
        fs::read_to_string(&out).is_ok_and(|s| s.lines().count() == 6)
    });
    let out = fs::read_to_string(out).unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "repo");
    assert_eq!(lines[1], "feature");
    assert_eq!(lines[2], "worktree.removed");
    assert_eq!(Path::new(lines[3]), repo);
    assert_eq!(Path::new(lines[4]), worktree);
    assert_eq!(
        fs::canonicalize(lines[5]).unwrap(),
        fs::canonicalize(&repo).unwrap()
    );
    assert!(!worktree.exists());
}
