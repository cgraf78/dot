# shellcheck shell=bash
# Strict client configuration parser. This file is data, never shell code: the
# parser expands only documented HOME spellings and rejects unknown syntax
# before any extension or dependency provider can execute.

_dot_config_error() {
  printf 'dot: config: %s\n' "$*" >&2
  return 1
}

_dot_config_control_bytes() {
  local path=$1

  LC_ALL=C od -An -t u1 "$path" 2>/dev/null |
    awk '{ for (i = 1; i <= NF; i++) if (($i < 32 && $i != 10) || $i == 127) exit 1 }'
}

_dot_config_expand_path() {
  local value=$1 home suffix

  # shellcheck disable=SC2016,SC2088 # These are literal accepted spellings.
  case $value in
    '~' | '~/'* | '$HOME' | '$HOME/'* | '${HOME}' | '${HOME}/'*)
      home=${HOME:-}
      case $home in
        /) ;;
        '' | */ | *//* | */./* | */. | */../* | */.. | *$'\n'* | *$'\r'*)
          return 1
          ;;
        /*) ;;
        *) return 1 ;;
      esac
      case $value in
        '~' | '$HOME' | '${HOME}') value=$home ;;
        '~/'*) suffix=${value#\~/} ;;
        '$HOME/'*) suffix=${value#\$HOME/} ;;
        '${HOME}/'*) suffix=${value#\$\{HOME\}/} ;;
      esac
      if [[ -n ${suffix:-} ]]; then
        if [[ $home == / ]]; then
          value=/$suffix
        else
          value=$home/$suffix
        fi
      fi
      ;;
    *'$'* | *'~'*) return 1 ;;
  esac
  # A recognized leading token does not authorize a second expansion later in
  # the path. Keeping this check after expansion makes mixed-token input fail
  # closed instead of leaving policy for a later shell to interpret.
  case $value in
    *'$'* | *'~'*) return 1 ;;
  esac
  case $value in
    '' | / | */ | *//* | */./* | */. | */../* | */.. | *$'\n'* | *$'\r'*)
      return 1
      ;;
    /*) ;;
    *) return 1 ;;
  esac
  REPLY=$value
}

dot_config_load() {
  local config_path=${1:-} line key value size line_number=0 saw_value=false
  local seen_version=false seen_extension_api=false
  local seen_extensions_dir=false seen_dependency_provider=false

  DOT_CONFIG_VERSION=1
  DOT_EXTENSION_API=
  DOT_EXTENSIONS_DIR=
  DOT_DEPENDENCY_PROVIDER=none
  export DOT_CONFIG_VERSION DOT_EXTENSION_API DOT_EXTENSIONS_DIR
  export DOT_DEPENDENCY_PROVIDER

  if [[ -z "$config_path" ]]; then
    dot_xdg_path config dot/config ||
      _dot_config_error 'HOME does not provide an absolute config root' || return
    config_path=$REPLY
  fi
  [[ -e "$config_path" || -L "$config_path" ]] || return 0
  [[ -f "$config_path" && ! -L "$config_path" ]] ||
    _dot_config_error "not a regular file: $config_path" || return
  size=$(LC_ALL=C wc -c <"$config_path" 2>/dev/null | tr -d '[:space:]') ||
    _dot_config_error "cannot size file: $config_path" || return
  [[ "$size" =~ ^[0-9]+$ && "$size" -le 65536 ]] ||
    _dot_config_error "file exceeds 65536 bytes: $config_path" || return
  _dot_config_control_bytes "$config_path" ||
    _dot_config_error "contains control bytes: $config_path" || return

  while IFS= read -r line || [[ -n "$line" ]]; do
    line_number=$((line_number + 1))
    case $line in
      '' | \#*) continue ;;
      *\\) _dot_config_error "line $line_number uses a continuation" || return ;;
      *=*) ;;
      *) _dot_config_error "line $line_number is not key=value" || return ;;
    esac
    key=${line%%=*}
    value=${line#*=}
    [[ -n "$key" && "$key" != *[!a-z_]* ]] ||
      _dot_config_error "line $line_number has an invalid key" || return
    if [[ "$saw_value" == false && "$key" != version ]]; then
      _dot_config_error 'version=1 must be the first setting' || return
    fi
    saw_value=true

    case $key in
      version)
        [[ "$seen_version" == false ]] ||
          _dot_config_error 'duplicate version' || return
        seen_version=true
        [[ "$value" == 1 ]] ||
          _dot_config_error "unsupported version: $value" || return
        DOT_CONFIG_VERSION=1
        ;;
      extension_api)
        [[ "$seen_extension_api" == false ]] ||
          _dot_config_error 'duplicate extension_api' || return
        seen_extension_api=true
        [[ "$value" == 1 ]] ||
          _dot_config_error "unsupported extension_api: $value" || return
        DOT_EXTENSION_API=1
        ;;
      extensions_dir)
        [[ "$seen_extensions_dir" == false ]] ||
          _dot_config_error 'duplicate extensions_dir' || return
        seen_extensions_dir=true
        _dot_config_expand_path "$value" ||
          _dot_config_error "invalid extensions_dir: $value" || return
        DOT_EXTENSIONS_DIR=$REPLY
        ;;
      dependency_provider)
        [[ "$seen_dependency_provider" == false ]] ||
          _dot_config_error 'duplicate dependency_provider' || return
        seen_dependency_provider=true
        case $value in
          none | shdeps) DOT_DEPENDENCY_PROVIDER=$value ;;
          *) _dot_config_error "unsupported dependency_provider: $value" || return ;;
        esac
        ;;
      *) _dot_config_error "unknown key: $key" || return ;;
    esac
  done <"$config_path"

  [[ "$seen_version" == true ]] ||
    _dot_config_error 'missing version=1' || return
  if [[ "$seen_extensions_dir" == true && "$DOT_EXTENSION_API" != 1 ]]; then
    _dot_config_error 'extensions_dir requires extension_api=1' || return
  fi
}
