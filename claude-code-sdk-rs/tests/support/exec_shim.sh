#!/bin/sh
# The executable a test plants when production code is going to *execute* what
# the test laid down (the fake `npm` of `cli_download`'s install tests). It is
# checked into the repository, so no test process ever opens it for writing.
#
# Why that matters: `cargo test` runs a binary's tests as threads of one
# process. `Command::spawn` forks that process, and the child inherits every
# descriptor open at that instant -- `O_CLOEXEC` closes the inherited copy at
# `execve`, not at `fork`. A file's write count lives on the inode, so while
# one thread's child is on its way to `execve`, another thread's `execve` of
# the script it had just written and closed is refused with ETXTBSY.
#
# A test plants a symlink to this file and writes the script it wants to run as
# a *data* file beside it, named after the symlink plus `.spec.sh`. `exec`
# replaces this shell, so stdin, stdout, stderr and the exit status of the spec
# are exactly those the caller would have seen had the spec itself been the
# executable.
exec /bin/sh "$0.spec.sh" "$@"
