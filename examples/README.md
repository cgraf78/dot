# Examples

- `minimal-dotfiles/` is a policy-free one-file client.
- `extension-dotfiles/` shows the strict config that enables a separate client
  extension library without placing policy in dot itself.
- `profile-dotfiles/` shows profiles selecting additional overlays on top of
  the always-active client.

Examples are synthetic public fixtures. `tests/config.rs` parses the extension
config, syntax-checks its hooks, and checks the minimal payload exists;
`tests/profiles.rs` resolves the profile example.
