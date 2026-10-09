//! Keeps the repository's own config from making git run a program.
//!
//! `.git/config`, `config.worktree`, the files they include, and the config
//! of each submodule belong to whatever wrote the repository: a coding
//! agent, an unpacked archive, a script. Git runs some of their settings as
//! commands while gitst only reads: `core.fsmonitor` and filter drivers on
//! every status and diff, `gpg.program` for a signed commit in the log, and
//! ssh, credential helpers, remote helpers, proxies, and hooks on fetch. Each git command gitst runs gets overrides
//! that put back the value from the user's own config (system, global, or
//! the environment) or turn the setting off.
//!
//! Overrides are passed as `GIT_CONFIG_KEY_n` / `GIT_CONFIG_VALUE_n`, which
//! a submodule's git inherits and which, unlike `-c`, take a name that
//! contains `=`. Git reads them from 2.31, the oldest gitst supports.
//!
//! The fetch overrides go on every command, not only `git fetch`: in a
//! partial clone, a status or diff fetches a missing blob by itself.
//!
//! Filter drivers and remote helpers are found by name in the config read
//! at the start of each snapshot, so a name added between that read and a
//! later command in the same snapshot is not covered until the next one.

use std::collections::BTreeSet;
use std::process::Command;

use super::parse::ConfigEntry;

/// Where hooks are looked for during a fetch: never a directory, so no
/// hook runs.
const NO_HOOKS: &str = "/dev/null";

/// The filter commands `git lfs install --local` writes. `git-lfs` comes
/// from `PATH`, so these run nothing the repository chose.
const LFS_FILTERS: [&str; 5] = [
    "git-lfs clean -- %f",
    "git-lfs smudge -- %f",
    "git-lfs smudge --skip -- %f",
    "git-lfs filter-process",
    "git-lfs filter-process --skip",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Guard {
    /// What any git command may run.
    common: Vec<(String, String)>,
    /// What a fetch may run or send, which any command may start.
    fetch: Vec<(String, String)>,
    /// Blank `GIT_PROXY_COMMAND` for `git fetch`, which turns off
    /// `core.gitProxy`: `-c` cannot, since its first matching entry wins.
    blank_proxy: bool,
    /// `GIT_CONFIG_COUNT` already in the environment; ours follow.
    env_count: usize,
}

/// System and global config, and config from the environment or the
/// command line: the user's own.
fn trusted(e: &ConfigEntry) -> bool {
    matches!(e.scope.as_str(), "system" | "global" | "command")
}

/// Whether `v` is a boolean, which for `core.fsmonitor` means git's own
/// daemon or none, not a hook.
fn is_bool(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "true" | "false" | "yes" | "no" | "on" | "off" | "1" | "0" | ""
    )
}

/// The driver name in `filter.<name>.clean`, `.smudge` or `.process`.
fn filter_name(key: &str) -> Option<&str> {
    let (name, var) = key.strip_prefix("filter.")?.rsplit_once('.')?;
    matches!(var, "clean" | "smudge" | "process").then_some(name)
}

/// A remote helper name git looks up as `git-remote-<name>` on `PATH`.
/// Anything else, such as `../x`, can name a program in the work tree.
fn plain_helper(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '_' | '.'))
}

fn is_helper_key(key: &str) -> bool {
    key.starts_with("credential.") && key.ends_with(".helper")
}

/// For a setting that can hand a fetch's traffic, credentials, or the
/// user's cookies to someone else: the key it falls back to when set for
/// one URL or remote, and the value when the user has set neither.
fn leaky_fetch_setting(key: &str) -> Option<(Option<String>, &'static str)> {
    let (generic, var) = if let Some(rest) = key.strip_prefix("http.") {
        match rest.rsplit_once('.') {
            Some((_, var)) => (Some(format!("http.{var}")), var),
            None => (None, rest),
        }
    } else if key.starts_with("remote.") && key.ends_with(".proxy") {
        (Some("http.proxy".to_string()), "proxy")
    } else {
        return None;
    };
    let default = match var {
        "sslverify" => "true",
        "proxy" | "sslcainfo" | "sslcapath" | "cookiefile" => "",
        _ => return None,
    };
    Some((generic, default))
}

fn push(out: &mut Vec<(String, String)>, key: &str, value: String) {
    out.push((key.to_string(), value));
}

/// Quotes `s` as one word for the shell git runs `core.sshCommand` with.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

impl Guard {
    /// Builds the overrides from the repository's config (in the order git
    /// listed it), its submodules' config, and the environment.
    pub fn new(
        config: &[ConfigEntry],
        submodules: &[ConfigEntry],
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Guard {
        let user = |key: &str| {
            config
                .iter()
                .rev()
                .find(|e| trusted(e) && e.key == key)
                .map(|e| e.value.clone())
        };
        let untrusted = || config.iter().filter(|e| !trusted(e));
        let mut common = Vec::new();

        // A boolean is git's own daemon or none; anything else is a hook.
        let fsmonitor = match config.iter().rev().find(|e| e.key == "core.fsmonitor") {
            Some(e) if trusted(e) || is_bool(&e.value) => e.value.clone(),
            _ => user("core.fsmonitor").unwrap_or_else(|| "false".into()),
        };
        push(&mut common, "core.fsmonitor", fsmonitor);
        // A signed commit makes `git log` and `git show` run `gpg.program`,
        // and the repository can set both. gitst never shows a signature.
        push(&mut common, "log.showSignature", "false".into());

        let filters: BTreeSet<&str> = untrusted()
            .chain(submodules)
            .filter(|e| !LFS_FILTERS.contains(&e.value.as_str()))
            .filter_map(|e| filter_name(&e.key))
            .collect();
        for name in filters {
            let mut off = true;
            for var in ["clean", "smudge", "process"] {
                let key = format!("filter.{name}.{var}");
                let value = user(&key).unwrap_or_default();
                off &= value.is_empty();
                push(&mut common, &key, value);
            }
            // A required filter with no command fails every status.
            if off {
                push(
                    &mut common,
                    &format!("filter.{name}.required"),
                    "false".into(),
                );
            }
        }

        let mut fetch = Vec::new();
        // `GIT_SSH_COMMAND` outranks config; then `core.sshCommand`, then
        // `GIT_SSH`.
        if env("GIT_SSH_COMMAND").is_none() {
            let ssh = user("core.sshcommand")
                .or_else(|| env("GIT_SSH").map(|p| sh_quote(&p)))
                .unwrap_or_else(|| "ssh".into());
            push(&mut fetch, "core.sshCommand", ssh);
        }
        // An empty helper clears the list, URL-specific helpers included;
        // then the user's own go back in their order.
        push(&mut fetch, "credential.helper", String::new());
        for e in config
            .iter()
            .filter(|e| trusted(e) && is_helper_key(&e.key))
        {
            push(&mut fetch, &e.key, e.value.clone());
        }
        let alternates = user("core.alternaterefscommand").unwrap_or_default();
        push(&mut fetch, "core.alternateRefsCommand", alternates);
        let ext = user("protocol.ext.allow").unwrap_or_else(|| "never".into());
        push(&mut fetch, "protocol.ext.allow", ext);
        let helpers: BTreeSet<&str> = untrusted()
            .filter(|e| e.key.starts_with("remote.") && e.key.ends_with(".vcs"))
            .map(|e| e.value.as_str())
            .filter(|v| !plain_helper(v))
            .collect();
        for name in helpers {
            push(
                &mut fetch,
                &format!("protocol.{name}.allow"),
                "never".into(),
            );
        }
        push(&mut fetch, "core.hooksPath", NO_HOOKS.into());
        // Not programs, but a way to see a fetch's traffic or to send the
        // user's cookies: each goes back to the user's own value, for the
        // same URL or remote, then in general, or to git's default.
        let leaky: BTreeSet<&str> = untrusted()
            .map(|e| e.key.as_str())
            .filter(|k| leaky_fetch_setting(k).is_some())
            .collect();
        for key in leaky {
            let Some((generic, default)) = leaky_fetch_setting(key) else {
                continue;
            };
            let value = user(key)
                .or_else(|| generic.as_deref().and_then(user))
                .unwrap_or_else(|| default.into());
            push(&mut fetch, key, value);
        }

        let local_proxy = untrusted().any(|e| e.key == "core.gitproxy");
        let blank_proxy =
            env("GIT_PROXY_COMMAND").is_none() && (local_proxy || user("core.gitproxy").is_none());

        Guard {
            common,
            fetch,
            blank_proxy,
            env_count: env("GIT_CONFIG_COUNT")
                .and_then(|n| n.trim().parse().ok())
                .unwrap_or(0),
        }
    }

    /// Reads the environment for `new`.
    pub fn env(key: &str) -> Option<String> {
        std::env::var(key).ok()
    }

    /// Adds the overrides for any git command.
    pub fn apply(&self, cmd: &mut Command) {
        let all: Vec<_> = self.common.iter().chain(&self.fetch).cloned().collect();
        self.set(cmd, &all);
    }

    /// Adds the overrides for `git fetch`.
    pub fn apply_fetch(&self, cmd: &mut Command) {
        self.apply(cmd);
        if self.blank_proxy {
            cmd.env("GIT_PROXY_COMMAND", "");
        }
    }

    fn set(&self, cmd: &mut Command, pairs: &[(String, String)]) {
        for (i, (key, value)) in pairs.iter().enumerate() {
            let n = self.env_count + i;
            cmd.env(format!("GIT_CONFIG_KEY_{n}"), key)
                .env(format!("GIT_CONFIG_VALUE_{n}"), value);
        }
        cmd.env(
            "GIT_CONFIG_COUNT",
            (self.env_count + pairs.len()).to_string(),
        );
    }
}

impl Default for Guard {
    /// The overrides with no repository config read yet: the defaults, so
    /// that no command runs unguarded before the first snapshot.
    fn default() -> Guard {
        Guard::new(&[], &[], &Guard::env)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(scope: &str, key: &str, value: &str) -> ConfigEntry {
        ConfigEntry {
            scope: scope.into(),
            key: key.into(),
            value: value.into(),
        }
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn value<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
        pairs
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn a_local_fsmonitor_hook_is_replaced_and_the_daemon_kept() {
        let g = |config: &[ConfigEntry]| Guard::new(config, &[], &no_env);
        let hook = entry("local", "core.fsmonitor", "./hook");
        assert_eq!(
            value(&g(std::slice::from_ref(&hook)).common, "core.fsmonitor"),
            Some("false")
        );
        let global = entry("global", "core.fsmonitor", "true");
        assert_eq!(
            value(&g(&[global.clone(), hook]).common, "core.fsmonitor"),
            Some("true"),
            "the user's own value comes back"
        );
        let daemon = entry("local", "core.fsmonitor", "true");
        assert_eq!(value(&g(&[daemon]).common, "core.fsmonitor"), Some("true"));
        let user_hook = entry("global", "core.fsmonitor", "/usr/local/bin/watchman-hook");
        assert_eq!(
            value(&g(&[user_hook]).common, "core.fsmonitor"),
            Some("/usr/local/bin/watchman-hook")
        );
        assert_eq!(value(&g(&[]).common, "core.fsmonitor"), Some("false"));
    }

    #[test]
    fn signatures_are_never_verified() {
        // `log.showSignature` runs `gpg.program` for a signed commit, and
        // both can come from the repository.
        let config = [
            entry("global", "log.showsignature", "true"),
            entry("local", "gpg.program", "./evil"),
        ];
        let g = Guard::new(&config, &[], &no_env);
        assert_eq!(value(&g.common, "log.showSignature"), Some("false"));
    }

    #[test]
    fn local_and_submodule_filters_are_turned_off() {
        let config = [
            entry("global", "filter.lfs.clean", "git-lfs clean -- %f"),
            entry("local", "filter.lfs.clean", "evil"),
            entry("local", "filter.a=b.process", "evil"),
            entry("local", "filter.ok.clean", "git-lfs clean -- %f"),
        ];
        let sub = [entry("command", "filter.sub.smudge", "evil")];
        let g = Guard::new(&config, &sub, &no_env);
        assert_eq!(
            value(&g.common, "filter.lfs.clean"),
            Some("git-lfs clean -- %f")
        );
        assert_eq!(value(&g.common, "filter.lfs.process"), Some(""));
        assert_eq!(value(&g.common, "filter.lfs.required"), None);
        for name in ["a=b", "sub"] {
            for var in ["clean", "smudge", "process"] {
                let key = format!("filter.{name}.{var}");
                assert_eq!(value(&g.common, &key), Some(""), "{key}");
            }
            let key = format!("filter.{name}.required");
            assert_eq!(value(&g.common, &key), Some("false"), "{key}");
        }
        assert_eq!(
            value(&g.common, "filter.ok.clean"),
            None,
            "a local LFS install stays"
        );
    }

    #[test]
    fn fetch_keeps_only_the_users_programs() {
        let config = [
            entry("system", "credential.helper", "osxkeychain"),
            entry("global", "credential.https://x.helper", "store"),
            entry("local", "credential.helper", "!evil"),
            entry("local", "credential.https://y.helper", "!evil"),
            entry("local", "core.sshcommand", "evil"),
            entry("local", "core.alternaterefscommand", "evil"),
            entry("local", "protocol.ext.allow", "always"),
            entry("local", "remote.origin.vcs", "../x"),
            entry("local", "remote.hg.vcs", "hg"),
        ];
        let g = Guard::new(&config, &[], &no_env);
        let helpers: Vec<(&str, &str)> = g
            .fetch
            .iter()
            .filter(|(k, _)| is_helper_key(k))
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            helpers,
            vec![
                ("credential.helper", ""),
                ("credential.helper", "osxkeychain"),
                ("credential.https://x.helper", "store"),
            ]
        );
        assert_eq!(value(&g.fetch, "core.sshCommand"), Some("ssh"));
        assert_eq!(value(&g.fetch, "core.alternateRefsCommand"), Some(""));
        assert_eq!(value(&g.fetch, "protocol.ext.allow"), Some("never"));
        assert_eq!(value(&g.fetch, "protocol.../x.allow"), Some("never"));
        assert_eq!(value(&g.fetch, "protocol.hg.allow"), None);
        assert_eq!(value(&g.fetch, "core.hooksPath"), Some(NO_HOOKS));
        assert!(g.blank_proxy);
    }

    #[test]
    fn a_fetch_keeps_the_users_tls_proxy_and_cookie_settings() {
        // Not programs, but a way to hand a background fetch's credentials
        // to someone else, or to send the user's cookies to the remote.
        let config = [
            entry("global", "http.sslverify", "true"),
            entry("global", "http.https://x.proxy", "http://corp:3128"),
            entry("local", "http.sslverify", "false"),
            entry("local", "http.https://x.sslverify", "false"),
            entry("local", "http.proxy", "http://evil:8080"),
            entry("local", "http.https://x.proxy", "http://evil:8080"),
            entry("local", "http.sslcainfo", "./ca.pem"),
            entry("local", "http.sslcapath", "./certs"),
            entry("local", "http.cookiefile", "/home/me/.cookies"),
            entry("local", "remote.origin.proxy", "http://evil:8080"),
        ];
        let g = Guard::new(&config, &[], &no_env);
        assert_eq!(value(&g.fetch, "http.sslverify"), Some("true"));
        assert_eq!(value(&g.fetch, "http.https://x.sslverify"), Some("true"));
        assert_eq!(value(&g.fetch, "http.proxy"), Some(""));
        assert_eq!(
            value(&g.fetch, "http.https://x.proxy"),
            Some("http://corp:3128"),
            "the user's own value comes back"
        );
        assert_eq!(value(&g.fetch, "http.sslcainfo"), Some(""));
        assert_eq!(value(&g.fetch, "http.sslcapath"), Some(""));
        assert_eq!(value(&g.fetch, "http.cookiefile"), Some(""));
        assert_eq!(value(&g.fetch, "remote.origin.proxy"), Some(""));
        // Only the user's: nothing to put back.
        let g = Guard::new(&config[..2], &[], &no_env);
        assert_eq!(value(&g.fetch, "http.sslverify"), None);
        assert_eq!(value(&g.fetch, "http.https://x.proxy"), None);
    }

    #[test]
    fn ssh_follows_the_users_environment() {
        let local = [entry("local", "core.sshcommand", "evil")];
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                vars.iter()
                    .find(|(name, _)| *name == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        let g = Guard::new(&local, &[], &env(&[("GIT_SSH_COMMAND", "my-ssh")]));
        assert_eq!(
            value(&g.fetch, "core.sshCommand"),
            None,
            "the variable outranks config"
        );
        let g = Guard::new(&local, &[], &env(&[("GIT_SSH", "/opt/it's/plink")]));
        assert_eq!(
            value(&g.fetch, "core.sshCommand"),
            Some(r"'/opt/it'\''s/plink'")
        );
        let mut config = vec![entry("global", "core.sshcommand", "ssh -i k")];
        config.extend(local);
        let g = Guard::new(&config, &[], &env(&[("GIT_SSH", "plink")]));
        assert_eq!(value(&g.fetch, "core.sshCommand"), Some("ssh -i k"));
    }

    #[test]
    fn a_users_proxy_survives_only_without_a_local_one() {
        let global = entry("global", "core.gitproxy", "p for example.com");
        let local = entry("local", "core.gitproxy", "evil");
        assert!(!Guard::new(std::slice::from_ref(&global), &[], &no_env).blank_proxy);
        assert!(Guard::new(&[global, local], &[], &no_env).blank_proxy);
        let set = |k: &str| (k == "GIT_PROXY_COMMAND").then(|| "p".to_string());
        assert!(!Guard::new(&[], &[], &set).blank_proxy);
    }

    #[test]
    fn the_default_guard_has_the_defaults() {
        let g = Guard::default();
        assert_eq!(value(&g.common, "core.fsmonitor"), Some("false"));
        assert_eq!(value(&g.common, "log.showSignature"), Some("false"));
        assert_eq!(value(&g.fetch, "core.hooksPath"), Some(NO_HOOKS));
    }

    #[test]
    fn every_command_gets_the_fetch_overrides_too() {
        // A partial clone's status or diff fetches a missing blob itself.
        let g = Guard::new(&[], &[], &no_env);
        let mut cmd = Command::new("git");
        g.apply(&mut cmd);
        let values: Vec<String> = cmd
            .get_envs()
            .filter(|(k, _)| k.to_string_lossy().starts_with("GIT_CONFIG_KEY_"))
            .filter_map(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
            .collect();
        assert!(values.iter().any(|k| k == "core.sshCommand"), "{values:?}");
        assert!(values.iter().any(|k| k == "core.hooksPath"), "{values:?}");
    }

    #[test]
    fn overrides_follow_the_users_own_environment_entries() {
        let count = |k: &str| (k == "GIT_CONFIG_COUNT").then(|| "2".to_string());
        let g = Guard::new(&[], &[], &count);
        let mut cmd = Command::new("git");
        g.apply(&mut cmd);
        let envs: Vec<(String, String)> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.unwrap_or_default().to_string_lossy().into_owned(),
                )
            })
            .collect();
        let total = 2 + g.common.len() + g.fetch.len();
        assert!(envs.contains(&("GIT_CONFIG_COUNT".into(), total.to_string())));
        assert!(envs.contains(&("GIT_CONFIG_KEY_2".into(), "core.fsmonitor".into())));
        assert!(envs.contains(&("GIT_CONFIG_VALUE_2".into(), "false".into())));
    }
}
