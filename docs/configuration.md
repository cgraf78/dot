# Configuration

Dot reads `${resolved_config_home}/dot/config`, where a relative or empty
`XDG_CONFIG_HOME` falls back to `$HOME/.config`. The file is never evaluated as
shell code.

Example:

```text
version=1
extension_api=1
extensions_dir=$HOME/.local/lib/dotfiles
dependency_provider=shdeps
```

The first non-comment setting must be `version=1`. Keys may appear only once.
Supported keys are:

| Key | Values |
| --- | --- |
| `version` | `1` |
| `extension_api` | `1` |
| `extensions_dir` | Normalized absolute path after a leading `~`, `$HOME`, or `${HOME}` expansion |
| `dependency_provider` | `none` or `shdeps` |

`extensions_dir` requires `extension_api=1`. Unknown keys, control bytes,
continuations, duplicate keys, unsupported versions, and other variable
expansions fail before any provider or extension executes. The config must be
a regular non-symlink file no larger than 65,536 bytes. A HOME-based value
requires `HOME` itself to be a normalized absolute path; mixed expansion tokens
such as `$HOME/path/$OTHER` are rejected rather than partially expanded.
