#!/bin/sh
if [ "$2" = "--liquid" ]; then
    method="$3"
    shift_count=3
else
    method="$2"
    shift_count=2
fi

case "$method" in
    fail)
        printf 'failed args:' >&2
        printf ' %s' "$@" >&2
        exit 17
        ;;
    getbestblockhash)
        printf 'not-a-block-hash\n'
        ;;
    rpc_error)
        printf 'RPC response:\nerror code: -8\nerror message:\ninvalid caller value %s\n' "$4"
        ;;
    stderr_zero)
        printf 'rpc rejected caller value %s\n' "$4" >&2
        ;;
    generatetoaddress)
        printf '["5555555555555555555555555555555555555555555555555555555555555555","6666666666666666666666666666666666666666666666666666666666666666"]\n'
        ;;
    timeout)
        shift "$shift_count"
        marker="$1"
        sleep 1
        printf alive > "$marker"
        ;;
    json_number)
        printf '42\n'
        ;;
    unquoted_id)
        printf '7777777777777777777777777777777777777777777777777777777777777777\n'
        ;;
    void_result)
        ;;
    invalid_response)
        printf 'response-secret-material\n'
        ;;
    oversized_stdout)
        head -c 70000 /dev/zero | tr '\000' x
        ;;
    oversized_stderr)
        head -c 70000 /dev/zero | tr '\000' x >&2
        exit 18
        ;;
    *)
        printf 'unsupported fake method\n' >&2
        exit 19
        ;;
esac
