# Security and trust model

The convenience `curl | bash` installation trusts TLS and GitHub delivery for
the installer itself. It requires the GitHub CLI (`gh`), verifies the platform
archive's published checksum, and then requires a GitHub artifact attestation
for `cgraf78/dot` whose signer repository is `cgraf78/actions`. The checksum
detects corruption and binds the requested asset name; the independently
verified build provenance is the executable-release trust decision.

The installer publishes one immutable versioned release behind an atomic
stable link and refuses foreign destinations. Caller-supplied local archives
use their explicitly supplied checksum but are not treated as online attested
downloads. A selected Shdeps development checkout remains explicit source
trust for dependency-provider behavior; Dot validates its user-owned root,
bootstrap entrypoints, Git metadata, and official origin before treating that
checkout as executable developer input. Those identity checks are not a
recursive content sandbox.

Client configuration is parsed as data. Extension discovery is versioned and
rejects unsafe roots, path components, file types, ownership, modes, duplicate
identities, control bytes, unknown grammar, and unbounded input. Hook and doctor
extensions then run in a fresh worker Bash with only their documented API and
private temporary storage.

Test extensions are different by design: they are standalone executables that
run under their declared interpreter with normal user authority and the client
environment needed for integration coverage. Dot revalidates each executable
at launch, gives it private temp, cache, and state roots, closes stdin,
supervises its process session, and accepts completion only through the
versioned result reporter. These boundaries provide trust validation and
lifecycle isolation, not a sandbox against same-user code.

Before initialization, repository integration, or overlay publication, dot
inspects the complete candidate inventory against its dynamic control-plane
paths. The check covers lexical and physical containment, including a
symlinked parent into dot or provider state. Publication revalidates the
physical parent generation. The only public-command exception is the exact
tracked `support/client-launcher.sh` from this release. This legacy adapter
derives the standalone release root and validates its native binary and public
hook API directory before dispatch.

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
