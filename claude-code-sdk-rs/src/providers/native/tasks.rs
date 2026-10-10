//! The background tasks a native session runs through `nexus-tools` (contract §4
//! `background_tasks` / `task_update`, §10 `cancel_tools(task)`).
//!
//! Nothing here reads the text the model reads. Three structured sources feed the table:
//!
//! - the `structuredContent.background_task` of a `Bash` (`run_in_background`), `Monitor` or
//!   `TaskStop` result of the session's `nexus` server: `{id, kind, command, output_file, pid,
//!   status}` ([`reported`]). The first report of an id adds a task identified by `nexus-tools`'
//!   own id, tied to the `tool_call` that started it; a later one (a `TaskStop`) changes its
//!   status;
//! - the server's `notifications/message` of logger [`TASKS_LOGGER`] (`{task_id, event: "ended",
//!   status, exit_code}`): the end of a task, whatever ended it;
//! - `cancel_tools(task)`, which calls the server's `TaskStop` and records what it answers.
//!
//! A task's status only ever leaves `running`: once terminal it stays. An end that arrives
//! before the result that announces the task (a command that ends at once) is kept aside, a
//! bounded number of them, and applied when the task appears. Every change is followed by a
//! **complete** snapshot.

use std::collections::VecDeque;

use serde_json::Value;

use crate::agent::{BackgroundTask, BackgroundTaskKind, BackgroundTaskStatus};

/// Logger of the `nexus-tools` notification that says a background task ended.
pub(crate) const TASKS_LOGGER: &str = "nexus-tools/tasks";
/// Logger of the `nexus-tools` notifications that carry the output lines of a `Monitor`.
pub(crate) const MONITOR_LOGGER: &str = "nexus-tools/monitor";
/// Ends of unknown tasks kept aside, at most.
const MAX_EARLY_ENDS: usize = 256;

/// What `nexus-tools` reported about one task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reported {
    pub(crate) id: String,
    pub(crate) kind: BackgroundTaskKind,
    pub(crate) command: String,
    pub(crate) pid: Option<u32>,
    pub(crate) status: BackgroundTaskStatus,
}

/// A status in the contract's vocabulary (what `nexus-tools` writes); `None` for anything else.
pub(crate) fn parse_status(text: &str) -> Option<BackgroundTaskStatus> {
    Some(match text {
        "running" => BackgroundTaskStatus::Running,
        "completed" => BackgroundTaskStatus::Completed,
        "failed" => BackgroundTaskStatus::Failed,
        "killed" => BackgroundTaskStatus::Killed,
        _ => return None,
    })
}

/// The `background_task` of a result's `structuredContent`, if it has a usable one.
pub(crate) fn reported(structured: &Value) -> Option<Reported> {
    let task = structured.get("background_task")?;
    let id = task.get("id")?.as_str().filter(|id| !id.is_empty())?;
    Some(Reported {
        id: id.to_owned(),
        kind: match task.get("kind").and_then(Value::as_str) {
            Some("shell") => BackgroundTaskKind::Shell,
            Some("monitor") => BackgroundTaskKind::Monitor,
            _ => BackgroundTaskKind::Other,
        },
        command: task
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        pid: task
            .get("pid")
            .and_then(Value::as_u64)
            .and_then(|pid| u32::try_from(pid).ok()),
        status: task
            .get("status")
            .and_then(Value::as_str)
            .and_then(parse_status)?,
    })
}

/// The end of a task, from a server notification: `(task id, status)`. `None` for any other
/// notification.
pub(crate) fn ended(notification: &Value) -> Option<(String, BackgroundTaskStatus)> {
    if notification.get("method").and_then(Value::as_str) != Some("notifications/message") {
        return None;
    }
    let params = notification.get("params")?;
    if params.get("logger").and_then(Value::as_str) != Some(TASKS_LOGGER) {
        return None;
    }
    let data = params.get("data")?;
    if data.get("event").and_then(Value::as_str) != Some("ended") {
        return None;
    }
    let id = data.get("task_id")?.as_str()?.to_owned();
    let status = data
        .get("status")
        .and_then(Value::as_str)
        .and_then(parse_status)
        .filter(|status| *status != BackgroundTaskStatus::Running)?;
    Some((id, status))
}

/// One output line of a `Monitor`, from a server notification: `(task id, line)`.
pub(crate) fn monitor_line(notification: &Value) -> Option<(String, String)> {
    if notification.get("method").and_then(Value::as_str) != Some("notifications/message") {
        return None;
    }
    let params = notification.get("params")?;
    if params.get("logger").and_then(Value::as_str) != Some(MONITOR_LOGGER) {
        return None;
    }
    let data = params.get("data")?;
    let id = data.get("task_id")?.as_str()?.to_owned();
    let line = data.get("line")?.as_str()?.to_owned();
    Some((id, line))
}

/// The background tasks of one session, in order of appearance.
#[derive(Debug, Default)]
pub(crate) struct TaskTable {
    tasks: Vec<BackgroundTask>,
    early_ends: VecDeque<(String, BackgroundTaskStatus)>,
}

impl TaskTable {
    /// The complete list, what every `background_tasks` event carries.
    pub(crate) fn snapshot(&self) -> Vec<BackgroundTask> {
        self.tasks.clone()
    }

    pub(crate) fn get(&self, id: &str) -> Option<&BackgroundTask> {
        self.tasks.iter().find(|task| task.id == id)
    }

    /// Records a report of the server. A new id becomes a task started by `tool_call_id` at
    /// `now_ms`; a known one may only leave `running`. Returns whether the table changed.
    pub(crate) fn record(&mut self, reported: Reported, tool_call_id: &str, now_ms: u64) -> bool {
        if let Some(task) = self.tasks.iter_mut().find(|task| task.id == reported.id) {
            let mut changed = false;
            if task.pid.is_none() && reported.pid.is_some() {
                task.pid = reported.pid;
                changed = true;
            }
            if task.status == BackgroundTaskStatus::Running
                && reported.status != BackgroundTaskStatus::Running
            {
                task.status = reported.status;
                changed = true;
            }
            return changed;
        }
        let mut status = reported.status;
        if let Some(at) = self
            .early_ends
            .iter()
            .position(|(id, _)| *id == reported.id)
            && let Some((_, end_status)) = self.early_ends.remove(at)
            && status == BackgroundTaskStatus::Running
        {
            status = end_status;
        }
        self.tasks.push(BackgroundTask {
            id: reported.id,
            kind: reported.kind,
            description: reported.command,
            status,
            started_at_ms: Some(now_ms),
            tool_call_id: Some(tool_call_id.to_owned()),
            parent: None,
            pid: reported.pid,
        });
        true
    }

    /// A task ended with `status`. A running task takes it (`true`); a terminal one keeps its
    /// own; an unknown one is kept aside for when it appears.
    pub(crate) fn end(&mut self, id: &str, status: BackgroundTaskStatus) -> bool {
        match self.tasks.iter_mut().find(|task| task.id == id) {
            Some(task) if task.status == BackgroundTaskStatus::Running => {
                task.status = status;
                true
            },
            Some(_) => false,
            None => {
                if self.early_ends.len() >= MAX_EARLY_ENDS {
                    self.early_ends.pop_front();
                }
                self.early_ends.push_back((id.to_owned(), status));
                false
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn started(id: &str) -> Reported {
        reported(&json!({"background_task": {
            "id": id, "kind": "shell", "command": "sleep 30",
            "output_file": "/tmp/x.out", "pid": 4242, "status": "running"}}))
        .expect("a report")
    }

    #[test]
    fn a_report_is_read_from_the_structured_content_and_nothing_else() {
        let task = started("abc");
        assert_eq!(task.id, "abc");
        assert_eq!(task.kind, BackgroundTaskKind::Shell);
        assert_eq!(task.command, "sleep 30");
        assert_eq!(task.pid, Some(4242));
        assert_eq!(task.status, BackgroundTaskStatus::Running);
        assert_eq!(reported(&json!({"n": 1})), None);
        assert_eq!(reported(&json!({"background_task": {"id": ""}})), None);
        assert_eq!(
            reported(&json!({"background_task": {"id": "a", "status": "weird"}})),
            None
        );
    }

    #[test]
    fn a_task_appears_once_then_only_leaves_running() {
        let mut table = TaskTable::default();
        assert!(table.record(started("a"), "call_1", 7));
        let task = table.get("a").unwrap().clone();
        assert_eq!(task.tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(task.started_at_ms, Some(7));
        assert_eq!(task.description, "sleep 30");
        // The same report again (a `TaskStop` of a running task would differ): nothing moves.
        assert!(!table.record(started("a"), "call_2", 9));
        assert!(table.end("a", BackgroundTaskStatus::Killed));
        assert!(!table.end("a", BackgroundTaskStatus::Completed));
        let mut late = started("a");
        late.status = BackgroundTaskStatus::Completed;
        assert!(!table.record(late, "call_3", 11));
        let task = table.get("a").unwrap();
        assert_eq!(task.status, BackgroundTaskStatus::Killed);
        assert_eq!(task.tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(table.snapshot().len(), 1);
    }

    #[test]
    fn an_end_heard_before_the_task_is_applied_when_it_appears() {
        let mut table = TaskTable::default();
        assert!(!table.end("fast", BackgroundTaskStatus::Completed));
        assert!(table.snapshot().is_empty());
        assert!(table.record(started("fast"), "call_1", 1));
        assert_eq!(
            table.get("fast").unwrap().status,
            BackgroundTaskStatus::Completed
        );
    }

    #[test]
    fn notifications_are_read_by_logger_and_event() {
        let end = json!({"jsonrpc": "2.0", "method": "notifications/message", "params": {
            "level": "info", "logger": TASKS_LOGGER,
            "data": {"task_id": "a", "event": "ended", "status": "killed", "exit_code": null}}});
        assert_eq!(
            ended(&end),
            Some(("a".to_owned(), BackgroundTaskStatus::Killed))
        );
        let mut other = end.clone();
        other["params"]["logger"] = json!("somebody/else");
        assert_eq!(ended(&other), None);
        let line = json!({"jsonrpc": "2.0", "method": "notifications/message", "params": {
            "level": "info", "logger": MONITOR_LOGGER, "data": {"task_id": "m", "line": "hello"}}});
        assert_eq!(
            monitor_line(&line),
            Some(("m".to_owned(), "hello".to_owned()))
        );
        assert_eq!(monitor_line(&end), None);
        assert_eq!(ended(&line), None);
    }
}
