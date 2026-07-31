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
    *)
        printf 'unsupported fake method\n' >&2
        exit 19
        ;;
esac
