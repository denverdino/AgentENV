#!/usr/bin/env bash
# Root only collects a private trace instance; the supplied test command must
# delegate Cargo/VM execution to the repository's non-root capability runner.
set -euo pipefail
[[ $(id -u) == 0 && $# -gt 0 ]] || {
    echo 'usage (root): AENV_APF_TRACE_LOG=/isolated/log with-dragonball-apf-trace.sh <non-root test command...>' >&2
    exit 2
}
: "${AENV_APF_TRACE_LOG:?explicit isolated evidence path required}"
trace_root=/sys/kernel/tracing/instances
instance="$trace_root/aenv-m2-apf-$$"
[[ ! -e "$instance" && ! -e "$AENV_APF_TRACE_LOG" ]]
mkdir "$instance"
reader_pid=
cleanup() {
    printf '0\n' > "$instance/tracing_on"
    if [[ -n "$reader_pid" ]]; then
        kill "$reader_pid" 2>/dev/null || true
        wait "$reader_pid" 2>/dev/null || true
    fi
    rmdir "$instance"
}
trap cleanup EXIT
printf 'mono\n' > "$instance/trace_clock"
for event in kvm_async_pf_not_present kvm_async_pf_completed kvm_async_pf_ready; do
    printf '1\n' > "$instance/events/kvm/$event/enable"
done
printf '1\n' > "$instance/tracing_on"
cat "$instance/trace_pipe" > "$AENV_APF_TRACE_LOG" &
reader_pid=$!
while [[ ! -f "$AENV_APF_TRACE_LOG" ]]; do sleep 0.01; done
chmod 0644 "$AENV_APF_TRACE_LOG"
"$@"
