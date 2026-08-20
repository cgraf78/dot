# Security and trust model

The convenience `curl | bash` installation trusts TLS, GitHub delivery, the
protected `cgraf78/dot` main branch, and the cgraf78 account. A checksum fetched
from that same mutable channel would not add independent trust, so dot does not
present one as verification.

The installer validates and publishes one ordinary checkout, uses a shared
owner-recorded mutation lock with Shdeps, and refuses foreign destinations. A
selected Shdeps development checkout is explicit source trust. Dot validates
its user-owned root, bootstrap entrypoints, Git metadata, and official origin
to prevent accidental foreign selection, then treats the whole checkout as
executable developer input, including existing binaries and Cargo inputs.
Those identity checks are not a recursive content sandbox. `install.sh
--managed` bypasses and never executes that development target.

Client configuration is parsed as data. Extension discovery is versioned and
rejects unsafe roots, path components, file types, ownership, modes, duplicate
identities, control bytes, unknown grammar, and unbounded input before a fresh
worker Bash sources client code. Hook and doctor workers receive only their
documented API and private temporary storage; cancellation owns their process
groups and descendants.

Before initialization, repository integration, or overlay publication, dot
inspects the complete candidate inventory against its dynamic control-plane
paths. The check covers lexical and physical containment, including a
symlinked parent into dot or provider state. Publication revalidates the
physical parent generation. The only public-command exception is the exact
tracked `support/client-launcher.sh` from this release. The permanent launcher
derives the official Shdeps checkout root and requires the public library link
to resolve into that same checkout before dispatch.

Client materialization reapplies the effective process umask after filesystem
creation, including on filesystems whose inherited default ACL would otherwise
grant broader access. Initialization and staged overlay clones normalize
tracked content and the Git control tree before publication, then configure
future repository-authority writes to remain owner-only. Successful base and
overlay pulls validate and normalize only paths changed by that pull; a mode
normalization failure makes the update fail rather than extending authority to
later extension execution.

Initialization and overlay replacement use private, generation-bound recovery
records. Rollback removes or restores only the exact leaf, parent, staged, and
backup generations recorded before mutation. Tests materialize and recover
the durable process-crash phases. These guarantees do not claim power-loss
durability without filesystem `fsync`, or safety against a hostile process
running concurrently with the same user credentials.
