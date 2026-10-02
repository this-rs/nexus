#!/bin/sh
# The executable every fake `claude` CLI in this crate's tests is reached
# through. It is checked into the repository, so no test process ever opens it
# for writing; see `tests/support/fake_exec.rs` for why that matters.
#
# A test plants a symlink to this file and writes the script it wants to run as
# a *data* file beside it, named after the symlink plus `.spec.sh`. `exec`
# replaces this shell, so stdin, stdout, stderr and the exit status of the spec
# are exactly those the caller would have seen had the spec itself been the
# executable.
exec /bin/sh "$0.spec.sh" "$@"
