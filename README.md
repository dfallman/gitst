# Gitst
**A real-time, glanceable git status monitor for the terminal**

<p align="center">
  <img width="700" alt="gitst-transp" src="https://github.com/user-attachments/assets/582d783f-7925-439a-835d-0cf7bbc7ea0e" />
</p>

Gitst typically sits in one pane of a multi-pane terminal setup (using tools like Dantty, tmux, screen, herdr, WezTerm, iTerm2, etc.) next to for instance an editor, a coding agent, and a shell. Gitst shows in real-time what is happening to the repository as it happens: changed files, commits, pushes, fetches, merges, and rebases.

By design, Gitst **monitors** git, it doesn't **manipulate** it. This comes with a few benefits, especially for team work scenarios when there's lots of team activity on the repo, and also for personal use in conjunction with coding agents. Being a git monitor, Gitst never changes your git repository (apart from `git fetch`), and it never takes over git's `index.lock`, so it cannot (by design) get in the way or obstruct git commands run elsewhere. This is useful for at least two scenarios: 1) when you might want the coding agent to control git but still see what's going on, or 2), like me, you just want to control git manually.

## Install

With Homebrew (macOS and Linux):

```sh
brew install dfallman/tap/gitst
```

Prebuilt binaries for macOS, Linux, and Windows are on the
[releases page](https://github.com/dfallman/gitst/releases). Or build from source
with Cargo:

```sh
cargo install --git https://github.com/dfallman/gitst
```

Requires `git` on your `PATH`.

## Use

Run `gitst` inside a repository, or `gitst <path>`. If the directory is not a
repository yet, Gitst waits and picks it up once `git init` or `git clone` has
run there.

It does its best to adapt to the terminal window it operates in: sections collapse, borders give way to rules, and columns
drop out as the pane shrinks, down to a single `branch ↑1 ●` line. Mouse support is enabled by default, so you can easily click around in the interface, collapse and expand panels, and view details by clicking on items. Gitst is also entirely operable using its keyboard shortcuts (see below).

The dashboard has five sections:

- **Changes**: staged, unstaged, untracked, and conflicted files, with `+`/`−`
  line counts. A `•` marks a file that just changed.
- **Activity**: a timeline of commits, amends, checkouts, merges, rebases,
  resets, pulls, cherry-picks, pushes, fetches, staging, stashes, and file edits.
  Edits that come within a minute of each other merge into one row.
- **Commits**: recent history, with `↑` on commits not yet pushed.
- **Branches** and **Stashes**: folded by default.

Warnings appear under the header for possible secrets, conflicts, an
`index.lock` held for more than 10 seconds, failed fetches, and config errors.

### Secret warnings

Gitst looks for things that should not be pushed: `.env` files, SSH and other
private keys, key stores, credentials files, Terraform state, and tokens with a
recognisable prefix (AWS, GitHub, GitLab, Slack, Stripe, Google, Anthropic,
OpenAI, npm, and SendGrid). It checks untracked files, staged and unstaged
changes, and commits that are not pushed yet, and shows what it finds in a red
band under the header. Click the band, or press `s`, to list each finding with
a masked snippet; a finding opens its diff.

Gitst only warns. It never blocks a commit or a push and never changes a file,
and it checks everything offline. To silence a false alarm, add `gitst:allow`
(or `gitleaks:allow`) to the line, or list paths in `leak_allow`. Above
`numstat_max_files` changes, only file names are checked.

### Mouse

- **Click** a file, commit, branch, stash, or activity row to open it full-pane
  (diffs, commit details, ahead/behind lists). Click `‹ back`, press `Esc`, or
  right-click to return.
- **Click** a section title to fold it. The **wheel** scrolls what is under the pointer.
- **Click** `↻` (or press `f`) to fetch now.

### Keys

| Key | Dashboard | Detail view |
|---|---|---|
| `j` `k` / `↓` `↑` | move | move / scroll |
| `Enter` / `l` / `→` | open, or fold a section title | open |
| `Esc` / `h` / `←` / `Backspace` | clear selection | back |
| `Tab` / `Shift-Tab` | next / previous section | |
| `Space` | fold section | page down |
| `PgDn` / `PgUp` | | page down / up |
| `g` / `G` / `Home` / `End` | top / bottom | top / bottom |
| `w` | | wrap long lines |
| `f` | fetch now | fetch now |
| `s` | possible secrets | possible secrets |
| `?` | help | help |
| `q` / `Ctrl-C` | quit | quit |
| `Ctrl-Z` | suspend (`fg` resumes; not on Windows) | suspend |

### Options

```
gitst [PATH] [--no-fetch] [--interval <DURATION>]
gitst --version
```

- `--no-fetch` turns off background fetching. `↻` and `f` still fetch.
- `--interval 10m` sets the time between background fetches (`30s`, `5m`, `1h`).
- `-V` / `--version` prints the version.

## Config

Optional. Gitst reads the first of these that exists:
`$XDG_CONFIG_HOME/gitst/config.toml` (when that is set),
`%APPDATA%\gitst\config.toml` (Windows), and `~/.config/gitst/config.toml`.
All keys are optional; these are the defaults:

```toml
fetch_interval = "5m"               # "0" turns the timer off; ↻ and f still work
fetch_prune = false                 # pass --prune to background fetches
collapsed = ["branches", "stashes"] # sections that start folded
icons = "none"                      # "nerd" for Nerd Font icons
pulse_seconds = 10                  # how long a changed file stays marked
max_changes = 1000                  # most changed files to list
numstat_max_files = 500             # skip +/− counts above this many changes
leak_scan = true                    # warn about possible secrets
leak_allow = []                     # paths never warned about (.gitignore syntax)
```

Section names for `collapsed` are `changes`, `activity`, `commits`, `branches`,
and `stashes`. Unknown keys and invalid values are named in the UI and keep their
defaults; the rest of the file still applies.

## Notes

- Colours come from your terminal's own palette, so Gitst follows light and dark
  themes. When the terminal reports its background colour, Gitst uses it for
  subtle tints.
- Gitst follows `status.showUntrackedFiles`. When it is not set, untracked
  directories are listed file by file.
- Diffs are always read plain, whatever your `color.*`, `diff.external`, or pager
  settings. Untracked symlinks show their target, and FIFOs and devices are never
  read.
- Background fetches run without a terminal, so they can never prompt for a
  password. If a fetch fails, Gitst shows `fetch failed: auth` (or `offline`,
  `timeout`, or git's message) and retries with backoff, doubling from the fetch
  interval up to an hour (5, 10, 20, then 60 minutes by default). A fetch that
  runs longer than a minute is stopped.
- Pushes appear in Activity when they were made from this clone. Git only
  records them in the remote-tracking reflog.
- Changes are picked up by watching the working tree and `.git`, skipping
  ignored paths. If the watcher cannot start, Gitst polls every 3 seconds.

## How it's made
Gitst is written in Rust, with help from tools like Anthropic's Claude Code. I've been writing code for over 30 years, and working with coding agents has rekindled my sense of awe at what code can do. They let me move faster, try more ideas, and test them more thoroughly than I would on my own.

