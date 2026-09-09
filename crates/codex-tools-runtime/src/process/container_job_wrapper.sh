set -eu

[ "$#" -eq 4 ] || exit 125
job_id=$1
shell=$2
mode=$3
command=$4
case "$job_id" in tools-mcp-job-*) ;; *) exit 125 ;; esac
job_suffix=${job_id#tools-mcp-job-}
case "$job_suffix" in ''|*[!0-9a-f]*) exit 125 ;; esac
case "$shell" in /bin/bash|/bin/sh|bash|sh) ;; *) exit 125 ;; esac
case "$mode" in -lc|-c) ;; *) exit 125 ;; esac

TOOLS_MCP_JOB_ID=$job_id
export TOOLS_MCP_JOB_ID
setsid "$shell" "$mode" "$command" &
leader=$!

marked_pids() {
    for environment in /proc/[0-9]*/environ; do
        [ -r "$environment" ] || continue
        if (tr '\000' '\n' < "$environment") 2>/dev/null |
            grep -Fqx "TOOLS_MCP_JOB_ID=$job_id"; then
            pid=${environment#/proc/}
            pid=${pid%/environ}
            [ "$pid" = "$$" ] || printf '%s\n' "$pid"
        fi
    done
}

cleanup() {
    trap - EXIT HUP INT TERM
    # Existing workload descendants retain the marker; cleanup helpers must not.
    unset TOOLS_MCP_JOB_ID
    /bin/kill -TERM -- "-$leader" 2>/dev/null || true
    marked_pids | while IFS= read -r pid; do
        /bin/kill -TERM "$pid" 2>/dev/null || true
    done
    attempt=0
    while [ "$attempt" -lt 20 ] && marked_pids | grep -q .; do
        attempt=$((attempt + 1))
        sleep 0.05
    done
    /bin/kill -KILL -- "-$leader" 2>/dev/null || true
    marked_pids | while IFS= read -r pid; do
        /bin/kill -KILL "$pid" 2>/dev/null || true
    done
    wait "$leader" 2>/dev/null || true
}

trap cleanup EXIT HUP INT TERM
set +e
wait "$leader"
status=$?
set -e
cleanup
exit "$status"
