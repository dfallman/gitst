//! Rules for possible secrets: file names that usually hold one, and tokens
//! with a distinctive prefix. Pure: text in, masked findings out.

use std::path::Path;
use std::sync::LazyLock;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use regex::{Regex, RegexSet};

/// A line holding either marker is never reported.
const ALLOW_MARKERS: [&str; 2] = ["gitst:allow", "gitleaks:allow"];

/// `(id, label, pattern)`. Only tokens with a distinctive prefix: entropy
/// and keyword rules mostly find placeholders.
const CONTENT_RULES: &[(&str, &str, &str)] = &[
    (
        "private-key",
        "private key",
        r"-----BEGIN [A-Z ]*PRIVATE KEY( BLOCK)?-----",
    ),
    (
        "aws-access-key",
        "AWS access key",
        r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b",
    ),
    (
        "github-token",
        "GitHub token",
        r"\b(?:gh[pousr]_[A-Za-z0-9]{36}|github_pat_[A-Za-z0-9_]{82})\b",
    ),
    (
        "gitlab-token",
        "GitLab token",
        r"\bglpat-[A-Za-z0-9_-]{20,}",
    ),
    (
        "slack-token",
        "Slack token",
        r"\bxox[baprs]-[A-Za-z0-9-]{10,}",
    ),
    (
        "slack-webhook",
        "Slack webhook",
        r"hooks\.slack\.com/services/T[A-Za-z0-9_]+/B[A-Za-z0-9_]+/[A-Za-z0-9_]+",
    ),
    (
        "stripe-live-key",
        "Stripe live key",
        r"\b(?:sk|rk)_live_[A-Za-z0-9]{24,}",
    ),
    (
        "google-api-key",
        "Google API key",
        r"\bAIza[0-9A-Za-z_-]{35}\b",
    ),
    (
        "anthropic-key",
        "Anthropic key",
        r"\bsk-ant-[A-Za-z0-9_-]{32,}",
    ),
    (
        "openai-key",
        "OpenAI key",
        r"\bsk-(?:proj|svcacct|admin)-[A-Za-z0-9_-]{20,}",
    ),
    ("npm-token", "npm token", r"\bnpm_[A-Za-z0-9]{36}\b"),
    (
        "sendgrid-key",
        "SendGrid key",
        r"\bSG\.[A-Za-z0-9_-]{22}\.[A-Za-z0-9_-]{43}\b",
    ),
];

/// Suffixes that mark an `.env` file as a template.
const ENV_TEMPLATES: [&str; 5] = [".example", ".sample", ".template", ".dist", ".defaults"];
const SSH_KEYS: [&str; 4] = ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"];
/// Binary key stores, whose contents cannot be scanned.
const KEY_STORES: [&str; 5] = ["p12", "pfx", "jks", "keystore", "kdbx"];
const CREDENTIAL_FILES: [&str; 4] = [".git-credentials", ".netrc", ".pgpass", ".htpasswd"];
/// Longest masked snippet, in characters.
const MASK_MAX: usize = 24;

struct Compiled {
    /// Which rules match a line, in one pass.
    set: RegexSet,
    /// Each rule alone, to find what it matched.
    each: Vec<Regex>,
}

static COMPILED: LazyLock<Compiled> = LazyLock::new(|| Compiled {
    set: RegexSet::new(CONTENT_RULES.iter().map(|r| r.2)).expect("leak rules compile"),
    each: CONTENT_RULES
        .iter()
        .map(|r| Regex::new(r.2).expect("leak rule compiles"))
        .collect(),
});

/// A content rule's match on one line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub rule: &'static str,
    pub label: &'static str,
    pub line: u32,
    /// Masked.
    pub snippet: String,
}

/// Content rules over `(line number, text)` pairs: at most one hit per rule
/// per line. Lines with an allow marker are skipped, and so are matches
/// containing `EXAMPLE`, as in documentation keys.
pub fn scan_lines<'a>(lines: impl IntoIterator<Item = (u32, &'a str)>) -> Vec<Hit> {
    let c = &*COMPILED;
    let mut hits = Vec::new();
    for (line, text) in lines {
        if ALLOW_MARKERS.iter().any(|m| text.contains(m)) {
            continue;
        }
        for i in c.set.matches(text).iter() {
            let Some(m) = c.each[i].find(text) else {
                continue;
            };
            if m.as_str().contains("EXAMPLE") {
                continue;
            }
            let (rule, label, _) = CONTENT_RULES[i];
            hits.push(Hit {
                rule,
                label,
                line,
                snippet: mask(rule, m.as_str()),
            });
        }
    }
    hits
}

/// The filename rule a repo-relative path matches, as `(id, label)`.
pub fn check_path(path: &str) -> Option<(&'static str, &'static str)> {
    let path = path.trim_end_matches('/').to_ascii_lowercase();
    let name = path.rsplit('/').next().unwrap_or(&path);
    let is_env = name == ".env"
        || (name.starts_with(".env.") && !ENV_TEMPLATES.iter().any(|s| name.ends_with(s)));
    let is_key_store = name
        .rsplit_once('.')
        .is_some_and(|(stem, ext)| !stem.is_empty() && KEY_STORES.contains(&ext));
    if is_env {
        Some(("env-file", ".env file"))
    } else if SSH_KEYS.contains(&name) {
        Some(("ssh-private-key", "SSH private key"))
    } else if is_key_store {
        Some(("key-store", "key store"))
    } else if CREDENTIAL_FILES.contains(&name)
        || path == ".aws/credentials"
        || path.ends_with("/.aws/credentials")
    {
        Some(("credentials-file", "credentials file"))
    } else if name == "terraform.tfstate" || name.starts_with("terraform.tfstate.") {
        Some(("terraform-state", "Terraform state"))
    } else {
        None
    }
}

/// The first and last four characters with `•` between, at most
/// `MASK_MAX` long; a short match keeps only its first four. A private-key
/// header is not secret and is kept whole.
pub fn mask(rule: &str, matched: &str) -> String {
    if rule == "private-key" {
        return matched.to_string();
    }
    let chars: Vec<char> = matched.chars().collect();
    let n = chars.len();
    let head: String = chars.iter().take(4).collect();
    if n < 12 {
        return format!("{head}•••");
    }
    let tail: String = chars[n - 4..].iter().collect();
    let dots = (n - 8).min(MASK_MAX - 8);
    format!("{head}{}{tail}", "•".repeat(dots))
}

/// Whether `pattern` is a valid `leak_allow` entry (gitignore syntax).
pub fn valid_allow_pattern(pattern: &str) -> bool {
    GitignoreBuilder::new("").add_line(None, pattern).is_ok()
}

/// `leak_allow` as a matcher rooted at the repository. Invalid patterns are
/// skipped: the config warning has already named them.
pub fn allow_matcher(root: &Path, patterns: &[String]) -> Gitignore {
    let mut b = GitignoreBuilder::new(root);
    for p in patterns {
        let _ = b.add_line(None, p);
    }
    b.build().unwrap_or_else(|_| Gitignore::empty())
}

/// Whether a repo-relative path is covered by `leak_allow`.
pub fn is_allowed(allow: &Gitignore, path: &str) -> bool {
    allow.matched_path_or_any_parents(path, false).is_ignore()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fake secrets are assembled at run time so this file holds nothing a
    // scanner — GitHub's, or gitst watching its own repository — would flag.
    fn aws() -> String {
        format!("{}{}", "AK", "IAQ7LM2XRT5VBN8KWD")
    }

    fn rules(line: &str) -> Vec<&'static str> {
        scan_lines([(1, line)])
            .into_iter()
            .map(|h| h.rule)
            .collect()
    }

    #[test]
    fn each_content_rule_matches_a_realistic_token() {
        let cases: Vec<(&str, String)> = vec![
            (
                "private-key",
                format!("-----BEGIN RSA {}-----", "PRIVATE KEY"),
            ),
            ("private-key", format!("-----BEGIN {}-----", "PRIVATE KEY")),
            (
                "private-key",
                format!("-----BEGIN OPENSSH {}-----", "PRIVATE KEY"),
            ),
            (
                "private-key",
                format!("-----BEGIN PGP {} BLOCK-----", "PRIVATE KEY"),
            ),
            ("aws-access-key", format!("aws_access_key_id = {}", aws())),
            (
                "aws-access-key",
                format!("{}{}", "AS", "IAQ7LM2XRT5VBN8KWD"),
            ),
            (
                "github-token",
                format!("token: {}_{}", "ghp", "a1B2".repeat(9)),
            ),
            (
                "github-token",
                format!("{}_{}", "github_pat", "x".repeat(82)),
            ),
            ("gitlab-token", format!("{}-{}", "glpat", "y".repeat(26))),
            ("slack-token", format!("{}-{}", "xoxb", "1234567890-abcdef")),
            (
                "slack-webhook",
                format!(
                    "https://hooks.slack.com/services/{}/{}/{}",
                    "T0ABC123",
                    "B0DEF456",
                    "z".repeat(24)
                ),
            ),
            (
                "stripe-live-key",
                format!("{}_{}", "sk_live", "a".repeat(24)),
            ),
            ("google-api-key", format!("{}{}", "AIza", "b".repeat(35))),
            (
                "anthropic-key",
                format!("{}-{}", "sk-ant-api03", "c".repeat(40)),
            ),
            ("openai-key", format!("{}-{}", "sk-proj", "d".repeat(24))),
            ("npm-token", format!("{}_{}", "npm", "e".repeat(36))),
            (
                "sendgrid-key",
                format!("SG.{}.{}", "f".repeat(22), "g".repeat(43)),
            ),
        ];
        for (rule, line) in &cases {
            assert_eq!(rules(line), vec![*rule], "{line}");
        }
    }

    #[test]
    fn near_misses_do_not_match() {
        let misses = [
            format!("{}{}", "AK", "IAQ7LM2XRT5VBN8KW"),
            format!("{}{}", "ak", "iaq7lm2xrt5vbn8kwd"),
            format!("-----BEGIN {}-----", "CERTIFICATE"),
            format!("-----BEGIN {}-----", "PUBLIC KEY"),
            format!("{}_{}", "sk_test", "a".repeat(24)),
            format!("{}_{}", "ghp", "a".repeat(35)),
            format!("{}{}", "AIza", "b".repeat(34)),
            "password = hunter2".to_string(),
            // AWS's documentation key.
            format!("{}{}", "AK", "IAIOSFODNN7EXAMPLE"),
        ];
        for line in &misses {
            assert!(rules(line).is_empty(), "{line}");
        }
    }

    #[test]
    fn allow_markers_skip_the_line() {
        assert!(rules(&format!("{} # gitst:allow", aws())).is_empty());
        assert!(rules(&format!("{} // gitleaks:allow", aws())).is_empty());
    }

    #[test]
    fn one_hit_per_rule_per_line_with_line_numbers() {
        let two = format!("{} {}", aws(), aws());
        let npm = format!("{}_{}", "npm", "e".repeat(36));
        let hits = scan_lines([(3, two.as_str()), (7, "nothing"), (9, npm.as_str())]);
        let got: Vec<(&str, u32)> = hits.iter().map(|h| (h.rule, h.line)).collect();
        assert_eq!(got, vec![("aws-access-key", 3), ("npm-token", 9)]);
    }

    #[test]
    fn masking() {
        assert_eq!(mask("aws-access-key", &aws()), "AKIA••••••••••••8KWD");
        let long = format!("{}-{}", "sk-ant-api03", "c".repeat(60));
        let m = mask("anthropic-key", &long);
        assert_eq!(m.chars().count(), 24);
        assert!(m.starts_with("sk-a") && m.ends_with("cccc"), "{m}");
        assert_eq!(mask("slack-token", "xoxb-12345"), "xoxb•••");
        let header = format!("-----BEGIN RSA {}-----", "PRIVATE KEY");
        assert_eq!(mask("private-key", &header), header);
        let hit = &scan_lines([(1, aws().as_str())])[0];
        assert!(!hit.snippet.contains("Q7LM2XRT"), "{}", hit.snippet);
    }

    #[test]
    fn filename_rules() {
        let rule = |p: &str| check_path(p).map(|r| r.0);
        assert_eq!(rule(".env"), Some("env-file"));
        assert_eq!(rule("app/.env.local"), Some("env-file"));
        assert_eq!(rule(".env.production"), Some("env-file"));
        for p in [
            ".env.example",
            ".env.sample",
            "web/.env.template",
            ".env.dist",
            ".env.defaults",
            ".envrc",
            "env.txt",
        ] {
            assert_eq!(rule(p), None, "{p}");
        }
        assert_eq!(rule("home/.ssh/id_ed25519"), Some("ssh-private-key"));
        assert_eq!(rule("id_rsa.pub"), None);
        assert_eq!(rule("certs/store.p12"), Some("key-store"));
        assert_eq!(rule("vault.kdbx"), Some("key-store"));
        assert_eq!(rule("server.pem"), None);
        assert_eq!(rule("server.key"), None);
        assert_eq!(rule(".aws/credentials"), Some("credentials-file"));
        assert_eq!(rule("me/.aws/credentials"), Some("credentials-file"));
        assert_eq!(rule("docs/credentials"), None);
        assert_eq!(rule(".netrc"), Some("credentials-file"));
        assert_eq!(rule("infra/terraform.tfstate"), Some("terraform-state"));
        assert_eq!(rule("terraform.tfstate.backup"), Some("terraform-state"));
        assert_eq!(rule("untracked_dir/"), None);
    }

    #[test]
    fn allow_patterns() {
        let allow = allow_matcher(
            Path::new("/repo"),
            &["tests/fixtures/".into(), "*.pem.example".into()],
        );
        assert!(is_allowed(&allow, "tests/fixtures/keys/a.txt"));
        assert!(is_allowed(&allow, "x/server.pem.example"));
        assert!(!is_allowed(&allow, "src/main.rs"));
        assert!(!is_allowed(&allow_matcher(Path::new("/repo"), &[]), ".env"));
        assert!(valid_allow_pattern("tests/**"));
        assert!(!valid_allow_pattern("a{b"));
    }
}
