# Git CoAuthors for OpenIDEA

`inf1mum.git-coauthors` — [Git Mob](https://git-mob.github.io/)-style
co-authored commits for [OpenIDEA](https://github.com/l0ng-ai/openidea):
keep a roster of your teammates, pick the ones you are pairing or mobbing
with, and every picked person is credited on your next commit with a
`Co-authored-by: Name <email>` trailer. Nothing leaves your machine: no
network, no account, no telemetry — the roster and the current co-author
set are the plugin's own storage.

## What it does

- **Roster.** *Git CoAuthors: Add Teammate* adds a person as
  `Name <email>` (the email is the identity: adding the same email again
  renames the entry instead of duplicating it); *Git CoAuthors: Remove
  Teammates* removes any number, also un-picking them. The roster is
  sorted by name and persists in the plugin's storage, so it survives
  restarts and the host reclaiming the idle instance.
- **Picking.** *Git CoAuthors: Pick Co-authors* is a multi-select over the
  roster: what you confirm **is** the co-author set for your next commit
  (confirming with nothing checked picks nobody; dismissing changes
  nothing). With an empty roster the same command first asks for a first
  teammate, so one dialog is enough to get started.
- **Status bar.** The current set at a glance: `＋ co-authors` when nobody
  is picked, `co-authors: Ada, Bob, Cid` (or `…, Cid, +N` past three
  names) otherwise. The tooltip shows the exact trailers the next commit
  gets; clicking the item opens the picker. The item re-renders from
  storage on activation and on `vcs-changed`, so it comes back after an
  IDE start or an idle reclaim.
- **Trailers.** *Git CoAuthors: Add Co-authored-by Trailers* — also a
  button above the commit message (`commit.message` menu) in the Commit
  tool window and the Commit dialog — reads the commit being prepared
  (`pending-commit`) and writes the message back with the trailers
  appended (`set-commit-message`). Appending is **idempotent** and
  conservative:
  - a person is added only when their email is not already credited by
    any `Co-authored-by:` line in the message (compared
    case-insensitively, wherever the line sits);
  - the trailers join the message's last block when that block is already
    nothing but `token: value` lines (git's trailer block, e.g. a
    `Reviewed-by:` footer), otherwise they go into a new final block —
    the user's own text is never re-flowed;
  - if everyone picked is already credited, the message is not rewritten
    at all;
  - the host refuses to overwrite a message the user typed meanwhile
    (`stale`), and the plugin says so instead of touching anything: you
    review the message and commit yourself, as always.

## Commands (search for "Git CoAuthors")

| Command | Effect |
| --- | --- |
| *Pick Co-authors* | Choose the co-author set (multi-select; empty confirm clears) |
| *Add Teammate* | Add a `Name <email>` to the roster (same email renames) |
| *Remove Teammates* | Remove teammates; they stop being picked |
| *Clear Co-authors* | Pick nobody for the next commit |
| *Add Co-authored-by Trailers* | Append the trailers to the commit being prepared (also the button above the commit message) |

## Install

From the community marketplace (Settings › Plugins › Available › search
"Git CoAuthors"), or download `inf1mum.git-coauthors-0.1.0.oiplugin` from
[Releases](https://github.com/Inf1mum/openidea-git-coauthors/releases) and
use Settings › Plugins › *Install Plugin from Disk…*.

## Capabilities and why

The manifest requires exactly four capabilities — everything else (the
picker, the input dialog, the status bar item) needs no grant:

| Capability | Risk | Why Git CoAuthors needs it |
| --- | --- | --- |
| `vcs.read` | Low | Reads the commit being prepared (`pending-commit`) — the message the trailers are appended to. |
| `vcs.write` | Medium | Replaces the commit message with the trailers appended (`set-commit-message`). Nothing is committed by the plugin; you review and commit. |
| `storage` | Low | Persists the teammates roster and the active co-author set across restarts and idle reclaim. |
| `events` | Low | `vcs-changed` wakeups re-render the status bar item after the host recreated the instance. |

Git CoAuthors asks for no `project.*`, no `editor.*`, no `net:*`, no
`process:*`, no `events.input` — it never reads your files or keystrokes.

## Honest limits

- **No automatic trailers.** The IDE's plugin API offers no hook that runs
  at commit time with a write into the message — before-commit checks can
  only report problems, not edit the message. So the trailers are added by
  the button/command on the message you are preparing (Git Mob's
  "prepare" step); the IDE never commits behind your back this way. If a
  commit API gains such a hook, this plugin can adopt it.
- **One global co-author set.** The picked set is per IDE, not per
  repository — matching how pairing sessions usually run; the tooltip
  always shows exactly what the next commit gets.
- **Identity is the email.** Two roster entries with the same email are
  one person (the later name wins); a trailer the message already carries
  is never duplicated by email, even under a different name.
- **Roster size.** 100 teammates at most (a sanity cap; each entry is a
  name and an email), names and emails at most 120 characters.
- **The plugin API version.** The commit surface
  (`pending-commit`, `set-commit-message`, the `commit.message` menu) is
  OpenIDEA API 1.3, which is the manifest's `engines.openidea = "^1.3"`.

## Development

```sh
cargo test                 # logic, against the SDK's mock host
openidea-plugin build      # → dist/plugin.wasm + the install-time checks
```

The plugin depends on the
[`openidea-plugin` SDK](https://github.com/l0ng-ai/openidea/tree/main/sdk/rust/openidea-plugin)
(pinned to tag `v0.1.46`) and builds for `wasm32-wasip2`. The 30 unit
tests cover trailer appending (dedup by email, trailer-block placement,
untouched messages), the pick/add/remove/clear flows, the status bar
content, the commit-surface apply path (including idempotency, no-draft
and capability-denied paths) and activation/events re-rendering.

## License

[Apache-2.0](LICENSE).
