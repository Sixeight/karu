# karu

Clean up local Git branches and worktrees after the work is done.

karu checks Git history and GitHub pull requests, shows what it recommends deleting, and asks for confirmation.
It leaves remote branches unchanged.
The name comes from the Japanese verb 刈る (karu), meaning “to cut back.”

## Install

Requires Git and Rust 1.88 or later.
From a local checkout:

```sh
cargo install --path .
```

This installs `karu` and `git-karu` in `~/.cargo/bin`.
Alternatively, `make install` installs both in `~/.local/bin`.

Install and authenticate [`gh`](https://cli.github.com) to include GitHub pull request status.
karu also works without it.

## Usage

```sh
karu                 # Clean up the current repository
git karu             # Same command through Git
karu path/to/repo    # Clean up another repository
karu --dry-run       # Print recommendations as JSON without deleting
```

karu fetches from `origin`, checks local branches and worktrees, and gives each item a verdict:

| Verdict | Action |
| --- | --- |
| `delete` | Preselect in the interactive candidate list. |
| `ask` | Leave unchecked in the interactive candidate list. |
| `keep` | Leave it alone. |

The current branch is kept and hidden from the table.
Deletion starts after the selection is confirmed.
In a terminal, `delete` items start selected and `ask` items start unchecked. Use the candidate list to change the selection and confirm all deletions together. It shows each item's reason, unpushed commits, and worktree; wider terminals also show unique commits and pull request details. `d`, `l`, and `s` open the diff, log, and worktree status for the current item. With `--yes`, `delete` items are already confirmed and the list contains only `ask` items.

When the terminal cannot show the selector, karu keeps the group confirmation and asks about each `ask` item separately.

| Key | Action |
| --- | --- |
| `↑` / `↓` or `k` / `j` | Move through candidates. |
| Space | Toggle deletion for the current item. |
| Enter | Delete the selected items when the cursor is on the run row at the bottom. |
| `d` / `l` / `s` | Inspect the diff, commit log, or worktree status. |
| Esc / `q` | Keep all items in the selector. |
| Ctrl-C | Cancel before applying the selected deletions. |

Detail views scroll with the arrow keys; Enter, Esc, or `q` returns to the selector.
When the terminal cannot show the selector, karu uses line prompts: `y`, `Y`, or `yes` confirms deletion; Enter keeps the item.

### Options

| Option | Effect |
| --- | --- |
| `--yes` | Skip selection for `delete` items; `ask` items still need selection. |
| `--json`, `--dry-run` | Print a JSON array and delete nothing. |
| `--no-fetch` | Skip `git fetch --prune`. |
| `--force` | Keep deletion recommendations as `delete` even when the worktree has uncommitted changes. |
| `--no-jev` | Disable Jev for this run. |

`--force` does not skip deletion confirmation or change `keep` verdicts.
`--json` still fetches and uses Jev when enabled.
It cannot be combined with `--yes`.
Use `karu --help` or `git karu -h` for help.

## Deletion rules

karu keeps the current branch, the default branch, the branch in the primary worktree, branches matching `karu.keep`, and branches with an open pull request.

Unique commits are commits missing from the local default branch.
Git merge checks use that local branch, so keeping it up to date helps karu recognize completed work.

Other branches become deletion candidates when they diverged from the default branch and were merged back, their tip matches the head of a merged or closed pull request, or their only unique commits are merge commits.
Review checkouts from a finished pull request can also qualify when all unique commits exist on fetched refs and the branch has no live upstream.
A matching file tree alone does not prove that a branch was merged.

Branches still undecided after these checks get an `ask` verdict after seven idle days.
Idle time measures when the local branch ref last moved, using the reflog.
Without `--force`, deletion recommendations for dirty worktrees become unchecked `ask` items. Select them in the candidate list to confirm deletion, or confirm them individually when karu uses line prompts.
Jev can assess the remaining items when enabled; otherwise they are kept.

If the current branch has no commits, karu exits without fetching or deleting anything.

## Configuration

Run these commands inside the repository:

```sh
git config --add karu.keep 'release-*'  # Protect matching branches; repeat for more patterns
git config karu.staleDays 14           # Days before asking about idle branches (default: 7)
git config karu.fetchMaxAge 5m         # Reuse a recent fetch (disabled by default)
```

`karu.keep` supports `*` as a wildcard.
Set `karu.staleDays` to `0` to disable age-based prompts.
`karu.fetchMaxAge` accepts seconds or a number ending in `s`, `m`, or `h`.
Remote changes made after a reused fetch are not seen until the next fetch.

## Optional Jev support

[Jev](https://docs.typesafe.ai) assesses branches that the Git and pull request rules leave undecided.
It is off by default.
To enable it, set `TYPESAFE_API_KEY` in the environment and run:

```sh
git config karu.jev true
```

Jev scores whether work has landed, looks abandoned, or contains disposable commits.
karu maps those scores to `delete`, `ask`, or `keep`.
A Jev deletion recommendation becomes `ask` if any unique commits exist only locally.
If the key is missing or the request fails, undecided items are kept.

Requests go to `api.typesafe.ai` and include:

- Branch names, including the default branch name.
- The latest commit subject and up to eight unique commit subjects.
- Commit counts, a diffstat summary, the last commit date, and idle time.
- Whether the upstream branch is gone, plus the pull request state, title, and merge time.

Source code, diff contents, local paths, commit hashes, and remote URLs are not sent.
Names and subjects are sent unchanged, including any identifiers they contain.
`--no-jev` disables these requests even when Jev is configured.

## Deletion and recovery

karu removes selected branches with `git branch -D` and prints each deleted branch's commit hash.
While that commit is still available, restore the branch with:

```sh
git branch <branch-name> <commit-hash>
```

This does not restore uncommitted worktree changes.
Worktree files are usually removed in the background after the directory is renamed, so the original path becomes available before cleanup finishes.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Set `KARU_TIMING=1` to print timing details.
`TYPESAFE_BASE_URL` overrides the Jev endpoint, which tests use for a local server.

## Acknowledgments

karu was inspired by [gh-poi](https://github.com/seachicken/gh-poi), a GitHub CLI extension for cleaning up local branches.
Thanks to seachicken and the gh-poi contributors for their work.

## License

[MIT](LICENSE)
