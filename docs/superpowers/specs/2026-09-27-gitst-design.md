# gitst — design spec

Date: 2026-09-27
Status: approved design, pending implementation plan

## 1. Purpose

gitst is a Rust terminal UI that shows the live state of the git repository it
is started in. It is built to live permanently in a small pane of a multi-pane
terminal setup (Dantty, tmux, WezTerm, …) next to a coding agent such as Claude
Code and a regular shell.

The question gitst answers at a glance is **"what is happening to my repo right
now?"** — including changes an agent in another pane is making — not "let me
operate git". Existing tools (lazygit, gitui, gitu, tig) are interactive
clients designed to be driven in a large, focused pane; gitst is an ambient,
glanceable, always-on monitor that adapts to whatever pane size it gets.

### Success criteria

- Useful at 44×28 and still legible at ~24×8.
- Reflects working-tree, index, ref and remote changes within ~250 ms of them
  happening locally.
- Never interferes with git commands run in other panes (never takes
  `index.lock`, never prompts).
- ~0% CPU when nothing changes.
- Looks modern and polished (htop-level density, OpenCode-level polish) in both
  light and dark terminal themes.

## 2. Scope

### v1 (this spec)

- **Read-only monitor.** Clicks and keys only change what is displayed. The one
  exception is `git fetch` (updates remote-tracking refs only; never touches the
  working tree, index or local branches).
- Stacked, collapsible dashboard; full-pane detail views; mouse and keyboard.
- Background fetch on a timer plus click-to-fetch.

### Planned later (design must not preclude)

- Safe actions: stage/unstage a file, copy commit hash, open file in `$EDITOR`.
  These will appear as buttons in detail-view headers.
- GitHub PR and CI status via `gh`.
- Worktrees section; watching several repos.
- Packaging: Homebrew tap and release-plz, as used for spritz.

### Explicitly out of scope

- Commit, push, checkout, merge, rebase or any other repo-mutating operation
  beyond fetch.

## 3. Technical approach

- **Git access:** shell out to the `git` CLI and parse machine-readable output
  (`status --porcelain=v2 -z --branch`, `log` with explicit field separators,
  `reflog`, `diff --numstat -z`, `rev-parse`, `fetch`). This gives exact parity
  with the user's git config, fsmonitor and credential helpers. All calls go
  through a `GitBackend` trait so a hot path can later move to `gix` if
  profiling warrants it.
- **Every git invocation** runs as `git --no-optional-locks …` with
  `GIT_TERMINAL_PROMPT=0` and `GIT_OPTIONAL_LOCKS=0`, and with the repo path
  passed via `-C`.
- **Rendering and input:** `ratatui` 0.30 + `crossterm` 0.29 (mouse capture
  enabled).
- **Watching:** `notify` with a debouncer (~150 ms). Worktree events are
  filtered through the repo's ignore rules (`ignore` crate) so build output such
  as `target/` or `node_modules/` does not trigger refreshes. Inside the git dir
  only `HEAD`, `index`, `refs/**`, `logs/**`, `packed-refs`, and `*_HEAD` /
  `rebase-*` / `sequencer` state trigger refreshes. The git dir and common dir
  are resolved with `git rev-parse --git-dir --git-common-dir` so linked
  worktrees and submodules work.
- **Concurrency:** std threads and channels, no async runtime.
  - watcher thread → `RefreshNeeded` signals
  - git worker thread → performs refreshes and fetches, sends complete
    immutable `Snapshot`s plus derived events
  - UI thread → owns app state, draws, handles input; never blocks on git
  - Refreshes never overlap; signals arriving during a refresh coalesce into
    exactly one follow-up refresh.
- **Config:** optional `~/.config/gitst/config.toml`; every key has a default.
- **CLI:** `gitst [PATH] [--no-fetch] [--interval <duration>]`. `PATH`
  defaults to the current directory; gitst resolves the enclosing repo root.

## 4. Dashboard

### 4.1 Header (always visible)

- **Line 1:** branch name (or `@<short-hash>` when detached, or operation
  progress such as `rebase 3/7`), `↑ahead ↓behind` relative to upstream, a dirty
  indicator `●`, and a clickable fetch control `↻ 2m` showing the age of the
  last successful fetch (spinner while fetching).
- **Line 2 (when height allows):** upstream name, nearest tag with distance
  (e.g. `v0.1.9+4`), stash count.
- **Warning line (only when applicable, highest priority, red/yellow):**
  unresolved conflicts; merge/rebase/cherry-pick/revert/bisect in progress;
  stale `index.lock` (present for > 10 s); branch has no upstream; last fetch
  failed (with reason: auth / offline / timeout); git command failure.

### 4.2 Sections (priority order)

1. **Changes** — one row per path: two-letter XY status as in `git status -s`,
   path, `+added −removed` and a small diffstat meter. Conflicted paths first,
   then the rest sorted by path. Paths whose content changed in the last ~10 s
   show a fading `•` pulse. Renames show `old → new`. A clean tree shows a
   single `✓ clean` row. Section title shows count and total `+/−`.
2. **Activity** — one merged, newest-first timeline:
   - Seeded at startup and kept current from reflogs: HEAD reflog (commit,
     amend, checkout, merge, rebase, reset, pull, cherry-pick) and
     remote-tracking reflogs (`update by push` → **pushed**, fetch updates →
     **fetched**). Push detection is limited to pushes made from this clone.
   - Live events derived by diffing consecutive snapshots: file bursts
     coalesced into one row (e.g. `+3 files changed`, `src/app.rs +12 −2`),
     staged/unstaged transitions, stash push/pop, fetch results (including
     "no changes").
   - Rows show `HH:MM`, a verb, and a compact subject.
3. **Commits** — recent commits reachable from HEAD: short hash, subject,
   relative age; `↑` marker on commits not on upstream; branch and tag
   decorations; merge commits marked.
4. **Branches** — local branches, most recently committed first, each with
   `↑/↓` vs its upstream; current branch marked.
5. **Stashes** — index, message, age.

### 4.3 Height allocation

1. Header (and warning line if present) is placed first.
2. Each expanded section, in priority order, is guaranteed a minimum of 3 rows
   (title + 2 content rows) if space remains; sections that cannot get their
   minimum collapse to a one-line summary (`▸ Commits (12)`).
3. Leftover rows are distributed to Changes, then Activity, then Commits, each
   up to its content height.
4. A section with more content than rows scrolls independently with the mouse
   wheel and shows a `+N more` row.
5. If even the section titles do not fit, only the header is shown.

Clicking a section title (or pressing `space` on it) folds/unfolds it for the
session. `config.toml` sets which sections start folded.

### 4.4 Width adaptation

- Paths are truncated from the left, always preserving the file name
  (`…/src/app.rs`).
- Below ~36 columns: drop `+/−` numbers and meters.
- Below ~30 columns: drop relative ages.
- Minimum supported width: 24 columns. Below that, show a compact
  `branch ●` line only.

### 4.5 Not in a repository

Show a centered "not a git repository" message and keep watching the
directory; switch to the dashboard automatically after `git init` or
`git clone`.

## 5. Detail views

Opening a row replaces the dashboard with a full-pane detail view. The header
gains a clickable `‹ back`. Views form a stack (commit → file-in-commit).

- **File diff:** colored diff with hunk headers rendered as dim separator
  rules; vertical scroll; long lines clipped, with `w` toggling soft wrap.
  Staged and unstaged changes for the same path appear as two labeled blocks.
  Untracked files render as all-added. Binary files show
  `binary, 14 KB → 16 KB`.
- **Commit:** hash, author, date/age, full message, per-file `+/−` meter list;
  clicking a file opens that file's diff at that commit.
- **Branch:** commits ahead of and behind its upstream as two short lists;
  clicking one opens the commit view.
- **Stash:** as the commit view.
- **Live updates:** detail views re-render on new snapshots while preserving
  scroll position. A file diff whose path becomes clean shows `✓ no changes`.

## 6. Input

### Mouse

- Left click: open row / fold or unfold section / activate button (`↻`,
  `‹ back`, hint-bar items).
- Wheel: scroll the section or view under the pointer.
- Right click: back.
- Hover highlight when the terminal reports motion events.
- No drag interactions, so terminal text selection (usually Shift+drag) is
  unaffected.

Mouse hit-testing uses a **hit map** built during each draw: a list of screen
rectangles mapped to actions. A click is a lookup in the most recent hit map.

### Keyboard

| Key | Action |
|---|---|
| `j` / `k`, `↓` / `↑` | move selection |
| `Enter` / `l` | open |
| `Esc` / `h` / `Backspace` | back |
| `Tab` / `Shift+Tab` | next / previous section |
| `space` | fold / unfold section |
| `f` | fetch now |
| `w` | toggle wrap (diff views) |
| `g` / `G` | top / bottom |
| `?` | help overlay |
| `q` | quit |

Mouse and keyboard share one selection model.

## 7. Visual design

A first-class requirement: gitst should look modern and deliberate, in the
spirit of **htop** (dense, color-coded, meters, bottom hint bar) and
**OpenCode** (rounded borders, restrained accent, clean spacing).

- **Theme-adaptive colors:** base styling uses only the terminal's 16 ANSI
  colors plus bold/dim/reverse, so gitst follows the pane's light or dark theme
  automatically. When `terminal-colorsaurus` detects the background color, a
  small set of truecolor tints is derived from it (header band, selection and
  hover highlight, pulse fade). Detection failure falls back to pure ANSI.
- **Header band:** solid background band across the full width.
- **Sections:** rounded borders (`╭╮╰╯`) with the section title and summary
  embedded in the top border line.
- **Meters:** per-file green/red diffstat bars; section-level diffstat bar in
  the Changes title; ahead/behind gauge in the header when width allows.
- **Glyphs:** a consistent set: `▾ ▸ ● ↑ ↓ ✓ ⚠ ↻ ‹ •`. Optional Nerd Font
  icons for files and branches behind `icons = "nerd"` in config (default off).
- **Hint bar:** bottom row with clickable hints (`? help  f fetch  q quit`),
  hidden when height < 12.
- **Graceful degradation:** below ~36 columns or ~14 rows, box borders collapse
  to single title rules. Data always takes priority over decoration.

## 8. Module structure

```
src/
  main.rs          CLI args, terminal setup/teardown, panic hook
  config.rs        config.toml loading with defaults
  model.rs         Snapshot and its parts (immutable)
  git/
    mod.rs         GitBackend trait
    cli.rs         CliBackend: runs git, applies env/flags, timeouts
    parse.rs       pure parsers: porcelain v2, numstat, log, reflog, diff
  worker.rs        refresh/fetch loop, backoff, snapshot diffing → events
  activity.rs      merging reflog entries and live events; coalescing
  watch.rs         notify + ignore filtering → RefreshNeeded
  app.rs           view stack, selection, fold/scroll state, hit map, input
  ui/
    mod.rs         draw entry point
    layout.rs      pure height/width allocation
    theme.rs       ANSI base + derived tints
    dashboard.rs   header and sections
    detail.rs      diff / commit / branch / stash views
    widgets.rs     meters, bordered sections, hint bar
```

Each module has one responsibility and communicates through plain data types
(`Snapshot`, `Event`, `Action`). Parsers and layout are pure functions.

## 9. Error handling and edge cases

- **git command fails:** keep the last good snapshot, render it dimmed, and
  show the error on the warning line. Retry on the next refresh signal.
- **Fetch:** runs with a 60 s timeout (process killed on expiry). On failure,
  back off 5 → 10 → 20 → 60 min and show the reason. A successful manual
  fetch resets the backoff. Fetch never passes `--prune` unless
  `fetch_prune = true` in config.
- **git not installed:** full-pane error message.
- **Large repos:** Changes list capped at 1000 rows (`+N more`); per-file
  `+/−` computation skipped above a configurable file-count threshold.
- **Submodules:** shown as changed entries; not recursed into.
- **Terminal resize:** re-run layout; clamp scroll positions.
- **Panic:** panic hook restores the terminal (leave alternate screen, disable
  mouse capture and raw mode) before printing.

## 10. Configuration (defaults)

```toml
fetch_interval = "5m"     # "0" disables the timer; click/f still works
fetch_prune = false
collapsed = ["branches", "stashes"]
icons = "none"            # "none" | "nerd"
pulse_seconds = 10
max_changes = 1000
numstat_max_files = 500
```

## 11. Testing

- **Parser unit tests** against captured git output fixtures, including
  renames, paths with spaces/newlines/unicode, conflicts, detached HEAD,
  initial commit (unborn branch), and in-progress rebase.
- **Integration tests** on real temporary repositories created with the git
  CLI: commits, branches, merge conflict, stopped rebase, stash, and push to a
  local bare remote; assert on the resulting `Snapshot` (including push
  detection and ahead/behind).
- **Layout tests** for the pure allocator at 24×8, 30×12, 44×28 and 120×50.
- **Render snapshot tests** with ratatui's `TestBackend` and `insta`.
- **Manual verification** in real Dantty panes with light and dark themes.
