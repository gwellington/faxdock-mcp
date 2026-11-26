#!/bin/sh
set +x
set +a
set -eu

cd "$(dirname "$0")"
docker_bin=${FAX_DOCKER_BIN:-docker}
op_bin=${FAX_OP_BIN:-op}
unset fax_api_key

read_api_key() {
    if [ -n "${FAX_KEY_COMMAND:-}" ]; then
        case "$FAX_KEY_COMMAND" in
            /*) "$FAX_KEY_COMMAND" ;;
            *) return 1 ;;
        esac
    elif [ -s .telnyx-key-ref ]; then
        key_ref=$(cat .telnyx-key-ref) || return 1
        case "$key_ref" in
            op://*/*/*)
                unset OP_CONNECT_HOST OP_CONNECT_TOKEN
                "$op_bin" read "$key_ref"
                ;;
            *) return 1 ;;
        esac
    fi
}

{
    if ! fax_api_key=$(read_api_key </dev/null 2>/dev/null); then
        fax_api_key=''
        printf '%s\n' 'FaxDock could not read the Telnyx key from the configured credential provider.' >&2
    fi
    case "$fax_api_key" in
        *[!a-zA-Z0-9._-]*)
            fax_api_key=''
            printf '%s\n' 'FaxDock found an invalid Telnyx key format.' >&2
            ;;
    esac
    printf '%s\n' "$fax_api_key"
    unset fax_api_key OP_SERVICE_ACCOUNT_TOKEN OP_CONNECT_HOST OP_CONNECT_TOKEN TELNYX_API_KEY
    exec cat
} | (
    unset OP_SERVICE_ACCOUNT_TOKEN OP_CONNECT_HOST OP_CONNECT_TOKEN TELNYX_API_KEY TELNYX_FAX_API_KEY
    exec "$docker_bin" compose exec -T faxdock telnyx-fax mcp
)
