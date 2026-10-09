# Changelog

## Unreleased

### Security

- gitst no longer runs programs that the repository's own config names.
  `.git/config`, `config.worktree`, the files they include, and submodules'
  config can be written by whatever writes the repository, and git ran
  several of their settings while gitst only read: `core.fsmonitor` hooks and
  filter drivers on every refresh, `gpg.program` for a signed commit in the
  log, diff drivers (`textconv`) in an open diff, and `core.sshCommand`, credential helpers, `core.gitProxy`, remote helpers,
  `remote.<name>.uploadpack`, `core.alternateRefsCommand`, a widened
  `protocol.ext.allow`, and hooks on a background fetch. Each is now put back
  to your global or system value, or turned off. Background fetches also skip
  submodules and automatic maintenance.
- A background fetch keeps your own `http.sslVerify`, proxy, certificate
  authority, and cookie file settings. Set in the repository's config, they
  could route the fetch, and the credentials your helper gives it, through
  someone else's server, or send your cookies to the remote.
- Output read from git is capped, so a huge diff or patch can no longer take
  all memory. A diff over 16 MB says it is too large to show.

### Changed

- The secret scan covers commits on every local branch that no remote has, in
  a repository without a remote too, and commits pushed from this clone in the
  last day, to any remote ref, so a commit pushed before gitst saw it is
  still reported. Files
  over 1 MiB are left out of patches, as they are for untracked files.
- When part of the secret scan cannot run (a git error or timeout, a commit
  too large to read, or more than 50 unpushed commits in a repository with a
  remote), a `secret scan incomplete` warning says so, and the last findings
  are kept.
- An open diff reloads only when the refresh shows that its file, HEAD, or the
  branch changed, and details load on their own thread, so opening one never
  waits for a refresh.
- Git 2.31 or newer is needed: the guard's overrides reach git through
  `GIT_CONFIG_KEY_n`, which older gits ignore. Before 2.26 the repository's
  config cannot be read at all, and a `repo config not read` warning says
  so; a background fetch does not run then.
- The toolchain is pinned in `rust-toolchain.toml`, and CI checks
  dependencies against the RustSec advisory database.

### Fixed

- Quitting stops a refresh that is still running, along with any hook it
  started.
- A detail view opened in a pane under 24 columns wide is drawn, instead of
  the one-line summary.
- Warnings that do not fit share the last row instead of being dropped, and
  the dashboard is dimmed only while the error that dims it is on screen. A
  config warning is no longer replaced by a file-watching error.
- A burst of events in an ignored directory, such as a build, no longer holds
  back the refresh for an edit.
- On Windows, git starts suspended and runs only once it is in its job, so
  that nothing it starts can escape a stop.
- `git add` together with an edit in one refresh shows both a staging row and
  an edit row in Activity. An edit that keeps the line counts marks the file
  as changed.
- The Commits section's `↑N` counts every commit ahead of the upstream, not
  only those among the 50 loaded.
- `collapsed` names an unknown section in a warning, and `icons` takes any
  case and warns about other values.
- A time ahead of the clock shows as `in 3h`, not `now`.
- The waiting screen picks up `git init` at once.
- The scrollbar in a detail view no longer covers the last column of text.
- An HTTP fetch that fails for lack of credentials shows as `auth`.
- The release workflow gives the Homebrew tap token to git through `gh`,
  never in a URL.

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
