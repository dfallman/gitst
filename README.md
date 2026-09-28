# gitst

A real-time, glanceable interactive **git status monitor** typically used in a small terminal pane alongside your coding editor or IDE, your coding agent, and other terminal panes.

<p align="center">
  <img width="496" alt="gitst-front" src="https://github.com/user-attachments/assets/3a474e6b-669e-4591-80eb-9f74018e6213" />
</p>

Gitst is meant to sit in one pane of a multi-pane setup (Dantty, tmux, screen, herdr, WezTerm etc.) next to for instance a coding agent and a shell, and show what is happening to the repository as it happens: changed files, commits, pushes, fetches, merges and rebases.

<p align="center">
  <img width="600" alt="gitst" src="https://github.com/user-attachments/assets/6a59ecbd-b764-437c-99c5-8123279c66fc" />
</p>

Note that by design, gitst *monitors* git, it doesn't *manipulate* it. This is by design and comes with a few benefits, especially for use in conjunction with coding agents where you might either want the coding agent to control git or, like me, you want to control git manually. Apart from `git fetch`, gitst never changes your git repository, and it never takes over git's `index.lock`, so it cannot get in the way or obstruct git commands run elsewhere.

## Install

With Homebrew (macOS and Linux):

```sh
brew install dfallman/tap/gitst
```

Prebuilt binaries for macOS, Linux and Windows are on the
[releases page](https://github.com/dfallman/gitst/releases). Or build from source
with Cargo:

```sh
cargo install --git https://github.com/dfallman/gitst
```

Requires `git` on your `PATH`.

## Use

Run `gitst` inside a repository, or `gitst <path>`. If the directory is not a
repository yet, gitst waits and picks it up once `git init` or `git clone` has
run there.

It does its best to adapt to the terminal window it operates in: sections collapse, borders give way to rules, and columns
drop out as the pane shrinks, down to a single `branch ↑1 ●` line. Mouse support is enabled by default, so you can easily click around in the interface, collapse and expand panels, and view details by clicking on items. Gitst is also entirely operable using its keyboard shortcuts (see below).

The dashboard has five sections:

- **Changes**: staged, unstaged, untracked and conflicted files, with `+`/`−`
  line counts. A `•` marks a file that just changed.
- **Activity**: a timeline of commits, amends, checkouts, merges, rebases,
  resets, pulls, cherry-picks, pushes, fetches, staging, stashes and file edits.
  Edits that come within a minute of each other merge into one row.
- **Commits**: recent history, with `↑` on commits not yet pushed.
- **Branches** and **Stashes**: folded by default.

Warnings appear under the header for conflicts, an `index.lock` held for more
than 10 seconds, failed fetches and config errors.

### Mouse

- **Click** a file, commit, branch, stash or activity row to open it full-pane
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

Optional. gitst reads the first of these that exists:
`$XDG_CONFIG_HOME/gitst/config.toml` (when that is set),
`%APPDATA%\gitst\config.toml` (Windows) and `~/.config/gitst/config.toml`.
All keys are optional; these are the defaults:

```toml
fetch_interval = "5m"               # "0" turns the timer off; ↻ and f still work
fetch_prune = false                 # pass --prune to background fetches
collapsed = ["branches", "stashes"] # sections that start folded
icons = "none"                      # "nerd" for Nerd Font icons
pulse_seconds = 10                  # how long a changed file stays marked
max_changes = 1000                  # most changed files to list
numstat_max_files = 500             # skip +/− counts above this many changes
```

Section names for `collapsed` are `changes`, `activity`, `commits`, `branches`
and `stashes`. Unknown keys and invalid values are named in the UI and keep their
defaults; the rest of the file still applies.

## Notes

- Colours come from your terminal's own palette, so gitst follows light and dark
  themes. When the terminal reports its background colour, gitst uses it for
  subtle tints.
- gitst follows `status.showUntrackedFiles`. When it is not set, untracked
  directories are listed file by file.
- Diffs are always read plain, whatever your `color.*`, `diff.external` or pager
  settings. Untracked symlinks show their target, and FIFOs and devices are never
  read.
- Background fetches run without a terminal, so they can never prompt for a
  password. If a fetch fails, gitst shows `fetch failed: auth` (or `offline`,
  `timeout`, or git's message) and retries with backoff, doubling from the fetch
  interval up to an hour (5, 10, 20, then 60 minutes by default). A fetch that
  runs longer than a minute is stopped.
- Pushes appear in Activity when they were made from this clone. Git only
  records them in the remote-tracking reflog.
- Changes are picked up by watching the working tree and `.git`, skipping
  ignored paths. If the watcher cannot start, gitst polls every 3 seconds.

