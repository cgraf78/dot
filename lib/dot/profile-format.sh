# shellcheck shell=bash
# Shared syntax for profile definitions, selectors, and client defaults.

_dot_profile_identifier_valid() {
  [[ ${1:-} =~ ^[a-z][a-z0-9-]*$ ]]
}
