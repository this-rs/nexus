//! Process-tree tracking and signalling of the Claude Code CLI (contract §10,
//! decision A6).
//!
//! The CLI runs its tools as child processes; a `Bash` tool is a shell under the
//! CLI, a background task a shell left running. Cancelling tools without ending
//! the turn therefore means signalling the CLI's **descendants** and leaving the
//! CLI itself alone; cancelling one background task means signalling that task's
//! own subtree. Which process belongs to which task is a guess by elimination
//! ([`claim_pid`]): the descendants that appeared after the tool call, youngest
//! first — what the orchestrator did before the contract existed.
//!
//! The process table is read once per call with `ps` through the single
//! launcher of `transport::spawn` (an allowlist environment, like every process
//! an adapter starts); signals go through `libc::kill`. On a platform without
//! either, every function here answers "nothing".

use std::time::Duration;

/// The signal sent to cancel a tool: what Ctrl-C would send.
pub const SIGINT: i32 = 2;

/// How long the process table may take to come back before it is given up.
const PS_TIMEOUT: Duration = Duration::from_secs(5);

/// One process of the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessRow {
    /// Process identifier.
    pub pid: u32,
    /// Parent process identifier.
    pub ppid: u32,
    /// Seconds since the process started, as `ps` reports them (`etime`).
    pub elapsed_secs: u64,
}

/// Parses the `[[dd-]hh:]mm:ss` of `ps -o etime`; `None` for anything else.
pub fn parse_etime(text: &str) -> Option<u64> {
    let text = text.trim();
    let (days, clock) = match text.split_once('-') {
        Some((days, clock)) => (days.parse::<u64>().ok()?, clock),
        None => (0, text),
    };
    let mut parts = clock.split(':').map(|part| part.parse::<u64>());
    let (first, second, third) = (parts.next(), parts.next(), parts.next());
    if parts.next().is_some() {
        return None;
    }
    let (hours, minutes, seconds) = match (first, second, third) {
        (Some(Ok(m)), Some(Ok(s)), None) => (0, m, s),
        (Some(Ok(h)), Some(Ok(m)), Some(Ok(s))) => (h, m, s),
        _ => return None,
    };
    Some(days * 86_400 + hours * 3_600 + minutes * 60 + seconds)
}

/// Parses the output of `ps -A -o pid=,ppid=,etime=`: one process per line,
/// three whitespace-separated columns. A line that does not fit is skipped.
pub fn parse_process_table(text: &str) -> Vec<ProcessRow> {
    text.lines()
        .filter_map(|line| {
            let mut columns = line.split_whitespace();
            let pid = columns.next()?.parse().ok()?;
            let ppid = columns.next()?.parse().ok()?;
            let elapsed_secs = columns.next().and_then(parse_etime).unwrap_or(0);
            Some(ProcessRow {
                pid,
                ppid,
                elapsed_secs,
            })
        })
        .collect()
}

/// Every descendant of `root` in `table` (children, grandchildren…), breadth
/// first; `root` itself is not included.
pub fn descendants_in(table: &[ProcessRow], root: u32) -> Vec<ProcessRow> {
    let mut found: Vec<ProcessRow> = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for row in table {
            if row.ppid == parent
                && row.pid != parent
                && !found.iter().any(|known| known.pid == row.pid)
            {
                found.push(*row);
                frontier.push(row.pid);
            }
        }
    }
    found
}

/// The process to attribute to a task started between two readings of the
/// CLI's descendants: a pid of `after` that was not in `before`, the youngest
/// (smallest `etime`) when several appeared. `None` when nothing new appeared.
pub fn claim_pid(before: &[u32], after: &[ProcessRow]) -> Option<u32> {
    after
        .iter()
        .filter(|row| !before.contains(&row.pid))
        .min_by_key(|row| (row.elapsed_secs, row.pid))
        .map(|row| row.pid)
}

/// Reads the process table once. Empty when `ps` is missing, fails or times out.
#[cfg(unix)]
pub async fn process_table() -> Vec<ProcessRow> {
    use crate::transport::spawn::{EnvPolicy, isolated_command};
    let mut command = isolated_command("ps", &EnvPolicy::allowlist());
    command
        .args(["-A", "-o", "pid=,ppid=,etime="])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(PS_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => parse_process_table(&String::from_utf8_lossy(&output.stdout)),
        Ok(Err(error)) => {
            tracing::warn!(%error, "the process table could not be read");
            Vec::new()
        },
        Err(_) => {
            tracing::warn!("the process table took too long to come back");
            Vec::new()
        },
    }
}

/// Reads the process table once. Always empty off Unix.
#[cfg(not(unix))]
pub async fn process_table() -> Vec<ProcessRow> {
    Vec::new()
}

/// The descendants of `root`, with their age.
pub async fn descendant_rows(root: u32) -> Vec<ProcessRow> {
    descendants_in(&process_table().await, root)
}

/// The process identifiers of every descendant of `root`; `root` excluded.
pub async fn descendant_pids(root: u32) -> Vec<u32> {
    descendant_rows(root)
        .await
        .iter()
        .map(|row| row.pid)
        .collect()
}

/// Sends `signal` to `pid`; `true` when the kernel accepted it (the process
/// existed and was ours). A process already gone (`ESRCH`) is `false`, quietly.
#[cfg(unix)]
pub fn signal_pid(pid: u32, signal: i32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: `kill` takes two integers and touches no memory of ours.
    let status = unsafe { libc::kill(pid, signal) };
    if status == 0 {
        return true;
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::ESRCH) {
        tracing::warn!(pid, signal, %error, "signal refused");
    }
    false
}

/// Sends nothing: no signals off Unix.
#[cfg(not(unix))]
pub fn signal_pid(_pid: u32, _signal: i32) -> bool {
    false
}

/// Sends `signal` to every descendant of `root` — **not** to `root` itself, the
/// CLI, whose turn must go on. Returns the pids that took the signal.
pub async fn signal_descendants(root: u32, signal: i32) -> Vec<u32> {
    descendant_pids(root)
        .await
        .into_iter()
        .filter(|pid| signal_pid(*pid, signal))
        .collect()
}

/// Sends `SIGINT` to `root` **and** its descendants: the subtree of one
/// background task. Returns the pids that took the signal.
pub async fn signal_subtree(root: u32) -> Vec<u32> {
    let descendants = descendant_pids(root).await;
    let mut killed = Vec::with_capacity(descendants.len() + 1);
    if signal_pid(root, SIGINT) {
        killed.push(root);
    }
    killed.extend(
        descendants
            .into_iter()
            .filter(|pid| signal_pid(*pid, SIGINT)),
    );
    killed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: u32, ppid: u32, elapsed_secs: u64) -> ProcessRow {
        ProcessRow {
            pid,
            ppid,
            elapsed_secs,
        }
    }

    #[test]
    fn etime_is_read_in_its_four_shapes() {
        assert_eq!(parse_etime("00:05"), Some(5));
        assert_eq!(parse_etime("01:02:03"), Some(3_723));
        assert_eq!(parse_etime("2-01:02:03"), Some(176_523));
        assert_eq!(parse_etime("   12:00 "), Some(720));
        assert_eq!(parse_etime("12"), None);
        assert_eq!(parse_etime("a:b"), None);
        assert_eq!(parse_etime("1:2:3:4"), None);
        assert_eq!(parse_etime(""), None);
    }

    #[test]
    fn the_process_table_is_parsed_leniently() {
        let table = parse_process_table(
            "    1     0 10-00:00:01\n  100     1    01:00\n  200   100       05\nnot a row\n  300   200 00:00:07\n",
        );
        assert_eq!(
            table,
            [
                row(1, 0, 864_001),
                row(100, 1, 60),
                // A malformed etime does not drop the process, only its age.
                row(200, 100, 0),
                row(300, 200, 7),
            ]
        );
    }

    #[test]
    fn descendants_exclude_the_root_and_follow_every_generation() {
        let table = [
            row(1, 0, 0),
            row(10, 1, 0),
            row(11, 10, 0),
            row(12, 11, 0),
            row(20, 1, 0),
            // A cycle or a self-parent never loops.
            row(30, 30, 0),
        ];
        let pids: Vec<u32> = descendants_in(&table, 10).iter().map(|r| r.pid).collect();
        assert_eq!(pids, [11, 12]);
        assert!(descendants_in(&table, 12).is_empty());
        assert!(descendants_in(&table, 30).is_empty());
        let all: Vec<u32> = descendants_in(&table, 1).iter().map(|r| r.pid).collect();
        assert_eq!(all.len(), 4);
        assert!(!all.contains(&1));
    }

    #[test]
    fn the_claimed_pid_is_the_youngest_newcomer() {
        let after = [row(50, 1, 300), row(51, 1, 2), row(52, 1, 9)];
        assert_eq!(claim_pid(&[50], &after), Some(51));
        assert_eq!(claim_pid(&[50, 51, 52], &after), None);
        assert_eq!(claim_pid(&[], &[]), None);
        // Same age: the smaller pid, so the choice is deterministic.
        assert_eq!(claim_pid(&[], &[row(7, 1, 1), row(5, 1, 1)]), Some(5));
    }

    #[cfg(unix)]
    #[test]
    fn signalling_a_process_that_does_not_exist_is_false_and_quiet() {
        // Signal 0 checks existence without delivering anything; a pid above
        // every plausible pid_max is not ours.
        assert!(!signal_pid(u32::MAX - 1, 0));
        assert!(signal_pid(std::process::id(), 0), "we exist");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_process_table_lists_this_process_under_its_parent() {
        let table = process_table().await;
        let me = std::process::id();
        let Some(row) = table.iter().find(|row| row.pid == me) else {
            // `ps` is not available or refused in this sandbox: nothing to
            // assert against, and the functions answer "nothing", as documented.
            assert!(table.is_empty(), "a table without this process: {table:?}");
            return;
        };
        assert!(row.ppid > 0);
        // Other tests of this process spawn children in parallel, so the subtree
        // is not empty in general: assert on a child this test owns instead.
        let mut child = crate::transport::spawn::isolated_command(
            "sleep",
            &crate::transport::spawn::EnvPolicy::allowlist(),
        )
        .arg("30")
        .kill_on_drop(true)
        .spawn()
        .expect("sleep is available");
        let child_pid = child.id().expect("a running child has a pid");
        assert!(
            descendant_pids(me).await.contains(&child_pid),
            "the child of this test is a descendant of this process"
        );
        let _ = child.kill().await;
    }
}
