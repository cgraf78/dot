# Initialization

`dot init` will create or resume a separate client Git directory at
`~/.dotfiles` with `$HOME` as its explicit work tree. Initialization is a
durable transaction: the same URL and branch resume after interruption, while
a mismatched request fails without changing the recorded state.

After initialization every command, `dot doctor` included, first checks that
the client still matches the recorded identity: the same Git directory, one
`origin` naming the recorded repository, the recorded branch checked out, and
a work tree of `$HOME`. When it does not, the command refuses and names what
changed and the step that puts it back, for example a `git ... checkout main --`
after the branch was switched, or a `git ... config core.worktree` command.
For a Git directory that was replaced, it offers to adopt the replacement
(moving the completed record aside and rerunning `dot init`) or, for the
separate layout, to move it aside and clone fresh.

When the first convergence fails (an overlay that cannot be cloned, say),
`dot init` exits 1 and its last line names the unfinished transaction and the
command that finishes it, for example `dot init: initialization is incomplete
(stopped at phase converging); rerun 'dot init --branch main URL' to finish
it`. The checkout is already committed, so every command keeps working
meanwhile; `dot init --status` reports `initialization: incomplete` and
`dot doctor` warns `dot init did not finish` with the same step. Rerunning that
command resumes the transaction, and so does the next `dot update` that exits
cleanly: it re-verifies the client against the transaction record and
completes it exactly as `dot init` would. A client that no longer matches
keeps the transaction, which `dot doctor` then reports as stale, to be moved
aside. A transaction that stopped before its checkout committed is
left to `dot init`, which resumes it, or `dot init --rollback`.

Initialization normally honors the committed dependency provider while it
converges repositories, overlays, and extensions. Shared bootstrap environments
that install an explicit dependency set separately may set
`DOT_INIT_SKIP_PROVIDER=1` for one `dot init` invocation. The config is still
parsed, all non-provider convergence still runs, and the setting is neither
written to config nor retained by later invocations. The only accepted values
are `0` and `1`; other values fail before initialization state is changed.
