//! Optional `config.toml`, in `$XDG_CONFIG_HOME/gitst/`, `%APPDATA%\gitst\`
//! on Windows, or `~/.config/gitst/`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::de::DeserializeOwned;

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub fetch_interval: String,
    pub fetch_prune: bool,
    pub collapsed: Vec<String>,
    pub icons: String,
    pub pulse_seconds: u64,
    pub max_changes: usize,
    pub numstat_max_files: usize,
    pub leak_scan: bool,
    /// Paths never warned about, in gitignore syntax.
    pub leak_allow: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            fetch_interval: "5m".into(),
            fetch_prune: false,
            collapsed: vec!["branches".into(), "stashes".into()],
            icons: "none".into(),
            pulse_seconds: 10,
            max_changes: 1000,
            numstat_max_files: 500,
            leak_scan: true,
            leak_allow: Vec::new(),
        }
    }
}

impl Config {
    /// Reads the first config file that exists. A missing file gives the
    /// defaults; unknown keys and bad values keep their defaults and are
    /// named in a warning to show in the UI.
    pub fn load() -> (Config, Option<String>) {
        let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
        let platform = if cfg!(windows) {
            dirs::config_dir()
        } else {
            None
        };
        let home = dirs::home_dir();
        for path in config_paths(xdg.as_deref(), home.as_deref(), platform.as_deref()) {
            if let Ok(text) = std::fs::read_to_string(&path) {
                return Config::from_toml(&text);
            }
        }
        (Config::default(), None)
    }

    /// Parses a config file key by key, so that one typo does not cost the
    /// rest of the file.
    pub fn from_toml(s: &str) -> (Config, Option<String>) {
        let table: toml::Table = match toml::from_str(s) {
            Ok(t) => t,
            Err(e) => {
                let warning = format!("config.toml: {}", e.message().trim_end());
                return (Config::default(), Some(warning));
            }
        };
        let mut c = Config::default();
        let mut bad = Vec::new();
        for (key, value) in table {
            let ok = match key.as_str() {
                "fetch_interval" => match value.try_into::<String>() {
                    Ok(v) if parse_duration(&v).is_some() => {
                        c.fetch_interval = v;
                        true
                    }
                    _ => false,
                },
                "fetch_prune" => set(&mut c.fetch_prune, value),
                "collapsed" => set(&mut c.collapsed, value),
                "icons" => set(&mut c.icons, value),
                "pulse_seconds" => set(&mut c.pulse_seconds, value),
                "max_changes" => set(&mut c.max_changes, value),
                "numstat_max_files" => set(&mut c.numstat_max_files, value),
                "leak_scan" => set(&mut c.leak_scan, value),
                "leak_allow" => match value.try_into::<Vec<String>>() {
                    Ok(patterns) => {
                        for p in patterns {
                            if crate::leaks::valid_allow_pattern(&p) {
                                c.leak_allow.push(p);
                            } else {
                                bad.push(format!("leak_allow {p:?} (invalid)"));
                            }
                        }
                        true
                    }
                    Err(_) => false,
                },
                _ => {
                    bad.push(format!("{key} (unknown)"));
                    continue;
                }
            };
            if !ok {
                bad.push(format!("{key} (invalid)"));
            }
        }
        let warning = (!bad.is_empty()).then(|| format!("config.toml: ignored {}", bad.join(", ")));
        (c, warning)
    }

    pub fn fetch_interval(&self) -> Duration {
        parse_duration(&self.fetch_interval).unwrap_or(Duration::from_secs(300))
    }
}

/// Stores `value` in `slot` if it has the right type.
fn set<T: DeserializeOwned>(slot: &mut T, value: toml::Value) -> bool {
    match value.try_into() {
        Ok(v) => {
            *slot = v;
            true
        }
        Err(_) => false,
    }
}

/// Where a config file may be, in order: under `XDG_CONFIG_HOME` when set
/// (and absolute, as the spec requires), the platform's config directory,
/// then `~/.config`, which is the documented place and stays valid.
fn config_paths(xdg: Option<&Path>, home: Option<&Path>, platform: Option<&Path>) -> Vec<PathBuf> {
    let xdg = xdg.filter(|p| p.is_absolute());
    let mut paths: Vec<PathBuf> = [xdg, platform]
        .into_iter()
        .flatten()
        .chain(home.map(|h| h.join(".config")).as_deref())
        .map(|dir| dir.join("gitst").join("config.toml"))
        .collect();
    paths.dedup();
    paths
}

/// Parses `"0"`, `"90"` (seconds), `"30s"`, `"5m"` or `"1h"`. A value too
/// large to count in seconds is rejected.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let (num, unit) = match s.char_indices().last()? {
        (i, c) if c.is_ascii_alphabetic() => (&s[..i], c),
        _ => (s, 's'),
    };
    let n: u64 = num.parse().ok()?;
    let secs = match unit {
        's' => n,
        'm' => n.checked_mul(60)?,
        'h' => n.checked_mul(3600)?,
        _ => return None,
    };
    Some(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_duration("0"), Some(Duration::ZERO));
        assert_eq!(parse_duration(" 90 "), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("x"), None);
        assert_eq!(parse_duration("5d"), None);
        // Too large to count in seconds.
        assert_eq!(parse_duration(&format!("{}h", u64::MAX / 1000)), None);
    }

    #[test]
    fn config_paths_prefer_xdg_then_platform_then_home() {
        // Absolute on this platform: `/x`, or `C:\x` on Windows.
        let root = PathBuf::from(if cfg!(windows) { r"C:\" } else { "/" });
        let p = |s: &str| root.join(s);
        assert_eq!(
            config_paths(Some(&p("x")), Some(&p("h")), None),
            vec![p("x/gitst/config.toml"), p("h/.config/gitst/config.toml")]
        );
        // A relative XDG_CONFIG_HOME is invalid and ignored.
        assert_eq!(
            config_paths(Some(Path::new("rel")), Some(&p("h")), None),
            vec![p("h/.config/gitst/config.toml")]
        );
        assert_eq!(
            config_paths(None, Some(&p("h")), Some(&p("h/AppData"))),
            vec![
                p("h/AppData/gitst/config.toml"),
                p("h/.config/gitst/config.toml")
            ]
        );
    }

    #[test]
    fn partial_toml_keeps_defaults() {
        let (c, warning) = Config::from_toml("fetch_prune = true");
        assert_eq!(warning, None);
        assert!(c.fetch_prune);
        assert_eq!(c.fetch_interval, "5m");
        assert_eq!(c.collapsed, vec!["branches", "stashes"]);
        assert_eq!(c.fetch_interval(), Duration::from_secs(300));
    }

    #[test]
    fn bad_keys_are_dropped_one_by_one() {
        let (c, warning) =
            Config::from_toml("fetch_prune = true\ncolour = 1\nfetch_interval = \"soon\"\n");
        assert!(c.fetch_prune, "good keys still apply");
        assert_eq!(c.fetch_interval, "5m");
        let warning = warning.unwrap();
        assert!(
            warning.contains("colour") && warning.contains("fetch_interval"),
            "{warning}"
        );
        let (c, warning) = Config::from_toml("fetch_prune = 3\npulse_seconds = 4");
        assert_eq!((c.fetch_prune, c.pulse_seconds), (false, 4));
        assert!(warning.unwrap().contains("fetch_prune"));
        assert_eq!(Config::from_toml("pulse_seconds = 4").1, None);
    }

    #[test]
    fn broken_toml_gives_defaults_and_a_warning() {
        let (c, warning) = Config::from_toml("fetch_prune = ");
        assert_eq!(c, Config::default());
        assert!(warning.is_some());
    }

    #[test]
    fn leak_keys() {
        let d = Config::default();
        assert!(d.leak_scan && d.leak_allow.is_empty());
        let (c, warning) =
            Config::from_toml("leak_scan = false\nleak_allow = [\"tests/fixtures/\", \"a{b\"]");
        assert!(!c.leak_scan);
        assert_eq!(c.leak_allow, vec!["tests/fixtures/"]);
        let warning = warning.unwrap();
        assert!(
            warning.contains("leak_allow \"a{b\" (invalid)"),
            "{warning}"
        );
        let (c, warning) = Config::from_toml("leak_scan = 1\nleak_allow = \"x\"");
        assert!(c.leak_scan && c.leak_allow.is_empty());
        let warning = warning.unwrap();
        assert!(
            warning.contains("leak_scan") && warning.contains("leak_allow"),
            "{warning}"
        );
    }
}
