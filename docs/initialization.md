# Initialization

`dot init` will create or resume a separate client Git directory at
`~/.dotfiles` with `$HOME` as its explicit work tree. Initialization is a
durable transaction: the same URL and branch resume after interruption, while
a mismatched request fails without changing the recorded state.

The complete initialization, status, and rollback interface is implemented and
tested before the public repository is created. Until then this document marks
the intended contract rather than a published command promise.
