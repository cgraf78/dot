# shellcheck shell=bash
# Public standalone-dot library ABI. Consumers must check this value before
# relying on any exported function; repository visibility does not make the
# private runtime modules an API. api-v1.tsv is the reviewed machine-readable
# inventory that prevents public names from appearing accidentally.

DOT_LIBRARY_API=1
export DOT_LIBRARY_API
