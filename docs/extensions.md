# Extensions

Extensions are disabled unless configuration declares both
`extension_api=1` and a normalized absolute `extensions_dir`.

Version 1 reserves these discovered collections beneath that directory:

- `merge-hooks.d/`
- `doctor.d/`

Other directories—including client-owned `git-hooks/`, `sley-hooks/`, helper
libraries, and tests—are ignored by the standalone engine. Hook filenames,
permissions, worker isolation, and public helper signatures are documented
alongside the hook implementation as it lands.
