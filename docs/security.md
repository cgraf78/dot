# Security and trust model

The convenience `curl | bash` installation trusts TLS, GitHub delivery, the
protected `cgraf78/dot` main branch, and the cgraf78 account. A checksum fetched
from that same mutable channel would not add independent trust, so dot does not
present one as verification.

The installer validates and publishes one ordinary checkout, uses a shared
owner-recorded mutation lock with Shdeps, and refuses foreign destinations. A
selected Shdeps development checkout is explicit source trust: it may be dirty
but its repository identity and tracked executable surfaces are revalidated.
`install.sh --managed` bypasses and never executes that development target.

Client configuration is parsed as data. Extension discovery is versioned and
the current parser rejects unsafe file types, control bytes, unknown grammar,
and unbounded input before any provider or extension can execute.

The remaining extension ownership checks, reserved-control-plane preflight,
and resumable initialization transaction are implementation gates for the next
extraction groups. Until those gates land, this local skeleton does not claim
those protections. The finished transaction design will cover tested SIGKILL
boundaries but will not claim power-loss durability without filesystem
`fsync`, or safety against a hostile process with the same user credentials.
