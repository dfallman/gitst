//! Optional `~/.config/gitst/config.toml`.

use std::time::Duration;

use serde::Deserialize;

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub fetch_interval: String,
    pub fetch_prune: bool,
    pub collapsed: Vec<String>,
    pub icons: String,
    pub pulse_seconds: u64,
    pub max_changes: usize,
    pub numstat_max_files: usize,
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
        }
    }
}

impl Config {
    /// Reads the config file. A missing file gives the defaults; an invalid
    /// one gives the defaults plus a warning to show in the UI.
    pub fn load() -> (Config, Option<String>) {
        let Some(path) = dirs::home_dir().map(|h| h.join(".config/gitst/config.toml")) else {
            return (Config::default(), None);
        };
        match std::fs::read_to_string(&path) {
            Err(_) => (Config::default(), None),
            Ok(text) => match Config::from_toml(&text) {
                Ok(c) => (c, None),
                Err(e) => (Config::default(), Some(format!("config.toml: {e}"))),
            },
        }
    }

    pub fn from_toml(s: &str) -> Result<Config, String> {
        let c: Config = toml::from_str(s).map_err(|e| e.message().to_string())?;
        if parse_duration(&c.fetch_interval).is_none() {
            return Err(format!("invalid fetch_interval \"{}\"", c.fetch_interval));
        }
        Ok(c)
    }

    pub fn fetch_interval(&self) -> Duration {
        parse_duration(&self.fetch_interval).unwrap_or(Duration::from_secs(300))
    }
}

/// Parses `"0"`, `"90"` (seconds), `"30s"`, `"5m"` or `"1h"`.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let (num, unit) = match s.char_indices().last()? {
        (i, c) if c.is_ascii_alphabetic() => (&s[..i], c),
        _ => (s, 's'),
    };
    let n: u64 = num.parse().ok()?;
    let secs = match unit {
        's' => n,
        'm' => n * 60,
        'h' => n * 3600,
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
    }

    #[test]
    fn partial_toml_keeps_defaults() {
        let c = Config::from_toml("fetch_prune = true").unwrap();
        assert!(c.fetch_prune);
        assert_eq!(c.fetch_interval, "5m");
        assert_eq!(c.collapsed, vec!["branches", "stashes"]);
        assert_eq!(c.fetch_interval(), Duration::from_secs(300));
    }

    #[test]
    fn bad_toml_errors() {
        assert!(Config::from_toml("fetch_prune = 3").is_err());
        assert!(Config::from_toml("fetch_interval = \"soon\"").is_err());
    }
}
