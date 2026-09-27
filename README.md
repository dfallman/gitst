# gitst

A live, glanceable git status monitor for a small terminal pane. 

Gitst is meant to sit in one pane of a multi-pane setup (Dantty, tmux, screen, herdr, WezTerm etc.)
next to for instance a coding agent and a shell, and show what is happening to the repository
as it happens: changed files, commits, pushes, fetches, merges and rebases. 

Note that by design, gitst is **read-only monitor**. The purpose of gitst is to *monitor* git, not *manipulate* it. 
This has a number of benefits, especially for use in conjunction with coding agents where you might either want the coding agent to 
control git or you want to control git manually. As gits is read only, apart from `git fetch`, it never changes your repository, 
and it never takes git's `index.lock`, so it cannot get in the way of git commands run in other panes. 

<p align="center">
  <img width="600" alt="gitst screenshot" src="https://github.com/user-attachments/assets/8eb005f3-d29d-4fd4-85c3-6c548edb6217" />
</p>

## Install

```sh
cargo install --path .
```

Requires `git` on your `PATH`.

## Use

Run `gitst` inside a repository, or `gitst <path>`. It adapts to the pane:
sections collapse, borders give way to rules, and columns drop out as the pane
shrinks, down to a single `branch ↑1 ●` line.

- **Click** a file, commit, branch, stash or activity row to open it full-pane
  (diffs, commit details, ahead/behind lists). Click `‹ back`, press `Esc`, or
  right-click to return.
- **Click** a section title to fold it. The **wheel** scrolls what is under the pointer.
- **Click** `↻` (or press `f`) to fetch now.

| Key | Action |
|---|---|
| `j` `k` / `↓` `↑` | move |
| `Enter` / `l` | open |
| `Esc` / `h` / `Backspace` | back |
| `Tab` / `Shift-Tab` | next / previous section |
| `Space` | fold section |
| `g` / `G` | top / bottom |
| `f` | fetch now |
| `w` | wrap long lines in diffs |
| `?` | help |
| `q` | quit |

Options: `--no-fetch` turns off background fetching, `--interval 10m` sets how
often it runs.

## Config

Optional, at `~/.config/gitst/config.toml`:

```toml
fetch_interval = "5m"             # "0" turns the timer off; ↻ and f still work
fetch_prune = false               # pass --prune to background fetches
collapsed = ["branches", "stashes"] # sections that start folded
icons = "none"                    # "nerd" for Nerd Font icons
pulse_seconds = 10                # how long a changed file stays marked
max_changes = 1000
numstat_max_files = 500           # skip +/− counts above this many changes
```

## Notes

- Colours come from your terminal's own palette, so gitst follows light and dark
  themes. When the terminal reports its background colour, gitst uses it for
  subtle tints.
- Background fetches run without a terminal, so they can never prompt for a
  password. If authentication is needed, gitst shows `fetch failed: auth` and
  retries with backoff (5, 10, 20, then 60 minutes).
- Pushes appear in Activity when they were made from this clone. Git only
  records them in the remote-tracking reflog.
