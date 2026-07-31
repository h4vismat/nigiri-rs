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
    getblockchaininfo)
        printf '%s\n' '{"chain":"regtest","blocks":101,"headers":101,"bestblockhash":"5555555555555555555555555555555555555555555555555555555555555555","bits":"207fffff","target":"7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff","difficulty":4.656542373906925e-10,"time":1700000000,"mediantime":1700000000,"verificationprogress":1.0,"initialblockdownload":false,"chainwork":"02","size_on_disk":4096,"pruned":false,"warnings":[]}'
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
        shift "$shift_count"
        printf 'response-secret-material %s\n' "$1"
        ;;
    oversized_stdout)
        head -c 70000 /dev/zero | tr '\000' x
        ;;
    oversized_stderr)
        head -c 70000 /dev/zero | tr '\000' x >&2
        exit 18
        ;;
    oversized_stdout_then_marker)
        shift "$shift_count"
        marker="$1"
        head -c 70000 /dev/zero | tr '\000' x
        sleep 1
        printf alive > "$marker"
        ;;
    oversized_stderr_then_marker)
        shift "$shift_count"
        marker="$1"
        head -c 70000 /dev/zero | tr '\000' x >&2
        sleep 1
        printf alive > "$marker"
        exit 18
        ;;
    long_stderr_secret)
        shift "$shift_count"
        printf 'rejected caller value %s\n' "$1" >&2
        exit 20
        ;;
    unicode_json)
        printf '\033[32m%s\033[0m\n' '{"message":"ação 日本語 🚀"}'
        ;;
    bounded_both_streams)
        head -c 60000 /dev/zero | tr '\000' o
        head -c 60000 /dev/zero | tr '\000' e >&2
        exit 21
        ;;
    *)
        printf 'unsupported fake method\n' >&2
        exit 19
        ;;
esac
