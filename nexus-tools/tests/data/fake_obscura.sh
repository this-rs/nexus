#!/bin/sh
# The stand-in for `obscura` in tests/search_obscura.rs and tests/server_binary.rs. Committed and
# executable, never written at test time: a script written then executed while another test thread
# forks fails on Linux with ETXTBSY ("Text file busy"). Each test links to it from its own
# directory and puts its behaviour in `body.sh` there, which is SOURCED, not executed.
here=$(dirname "$0")
for a in "$@"; do printf '%s\n' "$a"; done > "$here/argv"
env > "$here/env"
. "$here/body.sh"
