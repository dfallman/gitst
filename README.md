# gitst

A live, glanceable git status for a small terminal pane.

gitst is meant to sit in one pane of a multi-pane setup (Dantty, tmux, WezTerm…)
next to a coding agent and a shell, and show what is happening to the repository
as it happens: changed files, commits, pushes, fetches, merges and rebases. It is a
read-only monitor. Apart from `git fetch`, it never changes your repository, and it
never takes git's `index.lock`, so it cannot get in the way of git commands run in
other panes.

```
 main ↑2 ↓0 ●                          ↻ 2m
 origin/main · v0.1.9+4 · stash 1
╭ ▾ Changes (5) ────────────────── +64 −12 ╮
│ ?? notes.md                   +3 ■■      │
│  D old.rs                     −2 ■■      │
│  M src/app.rs             +42 −7 ■■■■■ • │
│  M src/git.rs              +9 −3 ■■■■    │
│ A  src/watch.rs              +10 ■■■■    │
╰──────────────────────────────────────────╯
╭ ▾ Activity ──────────────────────────────╮
│ 12:18 src/app.rs +42 −7               1m │
│ 12:17 commit 4de1e3c icons: jug on c… 2m │
│ 11:20 pushed origin/main 59eb9c3      1h │
╰──────────────────────────────────────────╯
╭ ▾ Commits ─────────────────────────── ↑2 ╮
│ 4de1e3c ↑ icons: jug on cream         2m │
│ ebf9d60 ↑ pin engine to v0.1.9        1h │
╰──────────────────────────────────────────╯
  ▸ Branches (4)
  ▸ Stashes (1)
 ? help  f fetch  q quit
```

## Install

```sh
cargo install --path .
```

Requires `git` on your `PATH`.

## Use

Run `gitst` inside a repository, or `gitst <path>`. If the directory is not a
repository yet, gitst waits and picks it up once `git init` or `git clone` has
run there.

It adapts to the pane: sections collapse, borders give way to rules, and columns
drop out as the pane shrinks, down to a single `branch ↑1 ●` line.

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

### Options

```
gitst [PATH] [--no-fetch] [--interval <DURATION>]
```

- `--no-fetch` turns off background fetching. `↻` and `f` still fetch.
- `--interval 10m` sets the time between background fetches (`30s`, `5m`, `1h`).

## Config

Optional, at `~/.config/gitst/config.toml`. All keys are optional; these are the
defaults:

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
and `stashes`. An invalid file is reported in the UI and the defaults are used.

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
