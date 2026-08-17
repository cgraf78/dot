# Initialization

`dot init` will create or resume a separate client Git directory at
`~/.dotfiles` with `$HOME` as its explicit work tree. Initialization is a
durable transaction: the same URL and branch resume after interruption, while
a mismatched request fails without changing the recorded state.

Initialization normally honors the committed dependency provider while it
converges repositories, overlays, and extensions. Shared bootstrap environments
that install an explicit dependency set separately may set
`DOT_INIT_SKIP_PROVIDER=1` for one `dot init` invocation. The config is still
parsed, all non-provider convergence still runs, and the setting is neither
written to config nor retained by later invocations. The only accepted values
are `0` and `1`; other values fail before initialization state is changed.
