#!/bin/sh
# Probe `maki -p --output-format stream-json` the way hrafnar drives it, and
# print stdout, stderr and the exit status separately. Re-run after a maki
# upgrade to re-check the stream shape the maki adapter decodes.
#
# usage: scripts/maki-probe.sh [resume <id>] [prompt <text>] [maki args...]
# The prompt goes to maki on stdin, as hrafnar sends it; default "say hi".
# `resume <id>` resumes that session instead of starting a fresh one.
set -u
id=$(python3 -c 'import uuid; print(uuid.uuid4())')
session_flag=--session-id
prompt='say hi'
while [ $# -ge 2 ]; do
    case $1 in
        resume) id=$2; session_flag=--resume; shift 2 ;;
        prompt) prompt=$2; shift 2 ;;
        *) break ;;
    esac
done
out_dir=target/maki-probe
mkdir -p "$out_dir"
printf '%s' "$prompt" > "$out_dir/prompt"
maki -p "$session_flag" "$id" --output-format stream-json "$@" \
    < "$out_dir/prompt" > "$out_dir/stdout" 2> "$out_dir/stderr"
status=$?
echo "maki: $(maki --version 2>&1)"
echo "session: $id"
echo "exit: $status"
echo "--- stdout"
cat "$out_dir/stdout"
echo "--- stderr"
cat "$out_dir/stderr"
