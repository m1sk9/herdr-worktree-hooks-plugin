use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const DEFAULT_FRESH_WINDOW_SECS: u64 = 300;

const BUILTIN_COPY: [&str; 2] = [".env", ".env.local"];

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    shell: Option<String>,
    fresh_window_secs: Option<u64>,
    defaults: Option<Section>,
    #[serde(default)]
    repos: BTreeMap<String, Section>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Section {
    #[serde(default)]
    pub copy: Vec<String>,
    #[serde(default)]
    pub run: Vec<String>,
    pub inherit: Option<bool>,
}

/// A repo's effective `copy` / `run` lists after merging `[defaults]`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Resolved {
    pub copy: Vec<String>,
    pub run: Vec<String>,
}

impl Config {
    /// A missing file is not an error: the built-in defaults are what makes the
    /// plugin useful with zero configuration.
    pub fn load(config_dir: Option<&Path>) -> Result<Self, String> {
        let Some(path) = config_dir.map(|d| d.join("config.toml")) else {
            return Ok(Self::default());
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn fresh_window_secs(&self) -> u64 {
        self.fresh_window_secs.unwrap_or(DEFAULT_FRESH_WINDOW_SECS)
    }

    /// Why not hardcode fish: the manifest is meant to be shareable, so the
    /// machine-specific shell stays in the user's config.toml.
    pub fn shell(&self) -> String {
        self.shell
            .clone()
            .or_else(|| std::env::var("SHELL").ok().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "/bin/sh".to_string())
    }

    pub fn resolve(&self, repo_name: &str, repo_root: &str) -> Resolved {
        let defaults = self.defaults.clone().unwrap_or_else(|| Section {
            copy: BUILTIN_COPY.iter().map(|s| s.to_string()).collect(),
            run: Vec::new(),
            inherit: None,
        });

        let Some(section) = self.section_for(repo_name, repo_root) else {
            return Resolved {
                copy: dedup(defaults.copy),
                run: dedup(defaults.run),
            };
        };

        if section.inherit == Some(false) {
            return Resolved {
                copy: dedup(section.copy.clone()),
                run: dedup(section.run.clone()),
            };
        }

        Resolved {
            copy: dedup([defaults.copy, section.copy.clone()].concat()),
            run: dedup([defaults.run, section.run.clone()].concat()),
        }
    }

    fn section_for(&self, repo_name: &str, repo_root: &str) -> Option<&Section> {
        let by_path = self.repos.iter().find(|(key, _)| {
            (key.starts_with('/') || key.starts_with('~'))
                && expand_tilde(key) == Path::new(repo_root)
        });
        by_path
            .map(|(_, section)| section)
            .or_else(|| self.repos.get(repo_name))
    }
}

fn dedup(entries: Vec<String>) -> Vec<String> {
    let mut seen = Vec::new();
    for entry in entries {
        if !seen.contains(&entry) {
            seen.push(entry);
        }
    }
    seen
}

fn expand_tilde(raw: &str) -> PathBuf {
    match raw.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => PathBuf::from(raw),
        },
        None => PathBuf::from(raw),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Config {
        toml::from_str(text).expect("valid config")
    }

    #[test]
    fn missing_config_file_yields_builtin_env_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(Some(dir.path())).unwrap();
        assert_eq!(
            config.resolve("anything", "/repo").copy,
            vec![".env", ".env.local"]
        );
    }

    #[test]
    fn absent_defaults_section_keeps_builtin_env_defaults() {
        let config = parse("[repos.my-app]\ncopy = [\".env.test\"]\n");
        assert_eq!(
            config.resolve("my-app", "/repo").copy,
            vec![".env", ".env.local", ".env.test"]
        );
    }

    #[test]
    fn explicit_empty_defaults_disables_builtin_env_defaults() {
        let config = parse("[defaults]\ncopy = []\n");
        assert!(config.resolve("my-app", "/repo").copy.is_empty());
    }

    #[test]
    fn repo_section_appends_to_defaults_without_duplicates() {
        let config = parse(
            "[defaults]\ncopy = [\".env\"]\nrun = [\"echo a\"]\n\
             [repos.my-app]\ncopy = [\".env\", \".env.test\"]\nrun = [\"echo b\"]\n",
        );
        let resolved = config.resolve("my-app", "/repo");
        assert_eq!(resolved.copy, vec![".env", ".env.test"]);
        assert_eq!(resolved.run, vec!["echo a", "echo b"]);
    }

    #[test]
    fn inherit_false_drops_defaults() {
        let config = parse(
            "[defaults]\ncopy = [\".env\"]\n\
             [repos.my-app]\ninherit = false\ncopy = [\".env.test\"]\n",
        );
        assert_eq!(config.resolve("my-app", "/repo").copy, vec![".env.test"]);
    }

    #[test]
    fn unmatched_repo_falls_back_to_defaults_only() {
        let config = parse("[defaults]\ncopy = [\".env\"]\n[repos.other]\ncopy = [\".secret\"]\n");
        assert_eq!(config.resolve("my-app", "/repo").copy, vec![".env"]);
    }

    #[test]
    fn absolute_path_key_matches_repo_root() {
        let config = parse(
            "[defaults]\ncopy = []\n\
             [repos.\"/home/dev/src/github.com/acme/my-app\"]\n\
             copy = [\".env.byroot\"]\n",
        );
        let resolved = config.resolve("my-app", "/home/dev/src/github.com/acme/my-app");
        assert_eq!(resolved.copy, vec![".env.byroot"]);
    }

    #[test]
    fn path_key_wins_over_name_key_for_same_repo() {
        let config = parse(
            "[defaults]\ncopy = []\n\
             [repos.my-app]\ncopy = [\".env.byname\"]\n\
             [repos.\"/home/dev/src/github.com/acme/my-app\"]\ncopy = [\".env.byroot\"]\n",
        );
        let resolved = config.resolve("my-app", "/home/dev/src/github.com/acme/my-app");
        assert_eq!(resolved.copy, vec![".env.byroot"]);
    }

    #[test]
    fn shell_defaults_to_configured_value() {
        let config = parse("shell = \"/bin/zsh\"\n");
        assert_eq!(config.shell(), "/bin/zsh");
    }

    #[test]
    fn fresh_window_defaults_when_unset() {
        let config = parse("");
        assert_eq!(config.fresh_window_secs(), DEFAULT_FRESH_WINDOW_SECS);
    }

    #[test]
    fn unknown_key_is_rejected_rather_than_silently_ignored() {
        assert!(toml::from_str::<Config>("copyy = [\".env\"]\n").is_err());
    }
}
