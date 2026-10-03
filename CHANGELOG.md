# Changelog

## 0.1.8 (2026-10-03)

### Added

- Secret warnings. gitst now looks for things that should not be pushed:
  `.env` files, SSH and other private keys, key stores, credentials files,
  Terraform state, and tokens with a recognisable prefix (AWS, GitHub, GitLab,
  Slack, Stripe, Google, Anthropic, OpenAI, npm, and SendGrid). It checks
  untracked files, staged and unstaged changes, and commits that are not pushed
  yet.
- A red band under the header shows what was found, and changed files and
  commits with a finding are marked in their rows.
- A findings view: click the band, or press `s`, to list each finding with a
  masked snippet. A finding opens its diff.
- `leak_scan` (default `true`) turns the scan on or off.
- `leak_allow` lists paths that are never warned about, in `.gitignore`
  syntax. Invalid patterns are reported as config errors.
- A line containing `gitst:allow` or `gitleaks:allow` is never reported.

gitst only warns. It never blocks a commit or a push and never changes a file,
and it checks everything offline. Above `numstat_max_files` changes, only file
names are checked.
