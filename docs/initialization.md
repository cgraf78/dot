# Initialization

`dot init` will create or resume a separate client Git directory at
`~/.dotfiles` with `$HOME` as its explicit work tree. Initialization is a
durable transaction: the same URL and branch resume after interruption, while
a mismatched request fails without changing the recorded state.

After initialization every command, `dot doctor` included, first checks that
the client still matches the recorded identity: the same Git directory, one
`origin` naming the recorded repository, the recorded branch checked out, and
a work tree of `$HOME`. When it does not, the command refuses and names what
changed and the step that puts it back, for example a `git ... checkout main`
after the branch was switched, or a `git ... config core.worktree` command.
For a Git directory that was replaced, it offers to adopt the replacement
(moving the completed record aside and rerunning `dot init`) or, for the
separate layout, to move it aside and clone fresh.

Initialization normally honors the committed dependency provider while it
converges repositories, overlays, and extensions. Shared bootstrap environments
that install an explicit dependency set separately may set
`DOT_INIT_SKIP_PROVIDER=1` for one `dot init` invocation. The config is still
parsed, all non-provider convergence still runs, and the setting is neither
written to config nor retained by later invocations. The only accepted values
are `0` and `1`; other values fail before initialization state is changed.
