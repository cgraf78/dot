# Overlays

Overlay descriptors live under `${resolved_config_home}/dot/overlays.d/`.
Git-backed and filesystem-backed overlays contribute a `home/` tree over the
base client repository. Group 5 implements and gates deterministic overlay
precedence, no-clobber ownership, and interrupted-link recovery without
embedding any particular client overlay. Until that group lands, this file
documents the intended interface rather than an available runtime feature.
