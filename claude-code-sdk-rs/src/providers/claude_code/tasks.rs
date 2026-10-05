//! The table of background tasks a Claude Code session keeps (contract §4,
//! `background_tasks` and `task_update`; decision A5).
//!
//! Three sources feed it, all through the session's pump:
//!
//! - a complete `tool_call` of `Bash` with `run_in_background: true`, or of
//!   `Monitor` ([`task_from_tool_call`]): a `shell` or `monitor` task identified
//!   by the tool call, `running`, its pid claimed a second later by
//!   [`super::cancel::claim_pid`];
//! - the CLI's own `background_tasks_changed` ([`TaskTable::merge`]), read
//!   leniently: a task it names is updated or added, one it does not name is kept;
//! - a `task_notification` with a terminal status ([`TaskTable::apply_notification`]).
//!
//! Every change is followed by a **complete** snapshot (`background_tasks`),
//! never a diff: the table is the only state, [`TaskTable::snapshot`] its only
//! view.

use serde_json::Value;

use crate::agent::{BackgroundTask, BackgroundTaskKind, BackgroundTaskStatus};

/// Status of a task from the CLI's vocabulary (`completed`, `failed`, `killed`
/// and their synonyms); anything else, or nothing, is `running`.
pub fn parse_status(text: &str) -> BackgroundTaskStatus {
    match text {
        "completed" | "done" | "success" | "succeeded" => BackgroundTaskStatus::Completed,
        "failed" | "error" | "errored" => BackgroundTaskStatus::Failed,
        "killed" | "cancelled" | "canceled" | "stopped" => BackgroundTaskStatus::Killed,
        _ => BackgroundTaskStatus::Running,
    }
}

/// Whether a status ends a task.
pub fn is_terminal(status: BackgroundTaskStatus) -> bool {
    !matches!(status, BackgroundTaskStatus::Running)
}

/// The background task a complete `tool_call` starts, if it starts one: `Bash`
/// with `run_in_background: true` (a `shell` task described by its
/// `description`, else its `command`) or `Monitor` (a `monitor` task). The task
/// is identified by the tool call itself; `started_at_ms` is the clock of the
/// caller.
pub fn task_from_tool_call(
    id: &str,
    name: &str,
    input: &Value,
    parent: Option<&str>,
    started_at_ms: u64,
) -> Option<BackgroundTask> {
    let kind = match name {
        "Bash" if input.get("run_in_background").and_then(Value::as_bool) == Some(true) => {
            BackgroundTaskKind::Shell
        },
        "Monitor" => BackgroundTaskKind::Monitor,
        _ => return None,
    };
    let text = |key: &str| {
        input
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    Some(BackgroundTask {
        id: id.to_owned(),
        kind,
        description: text("description")
            .or_else(|| text("command"))
            .unwrap_or_default(),
        status: BackgroundTaskStatus::Running,
        started_at_ms: Some(started_at_ms),
        tool_call_id: Some(id.to_owned()),
        parent: parent.map(str::to_owned),
        pid: None,
    })
}

/// The background tasks of one session, in order of appearance.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskTable {
    tasks: Vec<BackgroundTask>,
}

impl TaskTable {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// The complete list, what every `background_tasks` event carries.
    pub fn snapshot(&self) -> Vec<BackgroundTask> {
        self.tasks.clone()
    }

    /// Whether the table has no task at all.
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// The task with this identifier.
    pub fn get(&self, id: &str) -> Option<&BackgroundTask> {
        self.tasks.iter().find(|task| task.id == id)
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut BackgroundTask> {
        self.tasks.iter_mut().find(|task| task.id == id)
    }

    /// Adds a task, or replaces the one with the same identifier.
    pub fn start(&mut self, task: BackgroundTask) {
        match self.get_mut(&task.id) {
            Some(existing) => *existing = task,
            None => self.tasks.push(task),
        }
    }

    /// Merges what the CLI reports. A reported task is matched by `id`, else by
    /// `tool_call_id` (the CLI names a task by its own identifier, this adapter
    /// by the tool call that started it); a match takes the reported status, and
    /// the reported kind and description when they say something; no match adds
    /// the task. A task the CLI does not name is kept as it is.
    pub fn merge(&mut self, reported: Vec<BackgroundTask>) {
        for incoming in reported {
            let position = self
                .tasks
                .iter()
                .position(|task| task.id == incoming.id)
                .or_else(|| {
                    let call = incoming.tool_call_id.as_deref()?;
                    self.tasks
                        .iter()
                        .position(|task| task.tool_call_id.as_deref() == Some(call))
                });
            match position {
                Some(index) => {
                    let task = &mut self.tasks[index];
                    task.status = incoming.status;
                    if incoming.kind != BackgroundTaskKind::Other {
                        task.kind = incoming.kind;
                    }
                    if !incoming.description.is_empty() {
                        task.description = incoming.description;
                    }
                    if incoming.started_at_ms.is_some() {
                        task.started_at_ms = incoming.started_at_ms;
                    }
                    if incoming.pid.is_some() {
                        task.pid = incoming.pid;
                    }
                    if incoming.parent.is_some() {
                        task.parent = incoming.parent;
                    }
                },
                None => self.tasks.push(incoming),
            }
        }
    }

    /// Records the process of a task. `false` when the task is unknown.
    pub fn set_pid(&mut self, id: &str, pid: u32) -> bool {
        match self.get_mut(id) {
            Some(task) => {
                task.pid = Some(pid);
                true
            },
            None => false,
        }
    }

    /// Changes the status of a task. `false` when the task is unknown or already
    /// in that status.
    pub fn set_status(&mut self, id: &str, status: BackgroundTaskStatus) -> bool {
        match self.get_mut(id) {
            Some(task) if task.status != status => {
                task.status = status;
                true
            },
            _ => false,
        }
    }

    /// Marks `killed` every running task whose process is among `pids`. Returns
    /// how many tasks changed.
    pub fn mark_killed(&mut self, pids: &[u32]) -> usize {
        let mut changed = 0;
        for task in &mut self.tasks {
            if task.status == BackgroundTaskStatus::Running
                && task.pid.is_some_and(|pid| pids.contains(&pid))
            {
                task.status = BackgroundTaskStatus::Killed;
                changed += 1;
            }
        }
        changed
    }

    /// Applies a `task_notification`: the task named by `task_id`, else by
    /// `tool_call_id`, takes the notified status when that status is terminal.
    /// `true` when a task changed.
    pub fn apply_notification(
        &mut self,
        task_id: Option<&str>,
        tool_call_id: Option<&str>,
        status: Option<&str>,
    ) -> bool {
        let status = parse_status(status.unwrap_or_default());
        if !is_terminal(status) {
            return false;
        }
        let position = task_id
            .and_then(|id| self.tasks.iter().position(|task| task.id == id))
            .or_else(|| {
                let call = tool_call_id?;
                self.tasks
                    .iter()
                    .position(|task| task.tool_call_id.as_deref() == Some(call))
            });
        match position {
            Some(index) if self.tasks[index].status != status => {
                self.tasks[index].status = status;
                true
            },
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn shell(id: &str) -> BackgroundTask {
        task_from_tool_call(
            id,
            "Bash",
            &json!({"command": "sleep 30", "run_in_background": true, "description": "nap"}),
            None,
            1_000,
        )
        .unwrap()
    }

    #[test]
    fn only_a_background_bash_or_a_monitor_starts_a_task() {
        let task = shell("toolu_bg");
        assert_eq!(task.kind, BackgroundTaskKind::Shell);
        assert_eq!(task.description, "nap");
        assert_eq!(task.status, BackgroundTaskStatus::Running);
        assert_eq!(task.tool_call_id.as_deref(), Some("toolu_bg"));
        assert_eq!(task.started_at_ms, Some(1_000));
        assert_eq!(task.pid, None);

        let by_command = task_from_tool_call(
            "t",
            "Bash",
            &json!({"command": "make", "run_in_background": true}),
            Some("toolu_parent"),
            0,
        )
        .unwrap();
        assert_eq!(by_command.description, "make");
        assert_eq!(by_command.parent.as_deref(), Some("toolu_parent"));

        let monitor = task_from_tool_call(
            "m",
            "Monitor",
            &json!({"description": "watch the build"}),
            None,
            0,
        )
        .unwrap();
        assert_eq!(monitor.kind, BackgroundTaskKind::Monitor);
        assert_eq!(monitor.description, "watch the build");

        assert!(
            task_from_tool_call("f", "Bash", &json!({"command": "ls"}), None, 0).is_none(),
            "a foreground Bash is not a task"
        );
        assert!(
            task_from_tool_call(
                "f",
                "Bash",
                &json!({"command": "ls", "run_in_background": false}),
                None,
                0
            )
            .is_none()
        );
        assert!(task_from_tool_call("r", "Read", &json!({}), None, 0).is_none());
    }

    #[test]
    fn statuses_are_read_from_the_clis_vocabulary() {
        assert_eq!(parse_status("completed"), BackgroundTaskStatus::Completed);
        assert_eq!(parse_status("failed"), BackgroundTaskStatus::Failed);
        assert_eq!(parse_status("cancelled"), BackgroundTaskStatus::Killed);
        assert_eq!(parse_status("running"), BackgroundTaskStatus::Running);
        assert_eq!(parse_status(""), BackgroundTaskStatus::Running);
        assert!(!is_terminal(BackgroundTaskStatus::Running));
        assert!(is_terminal(BackgroundTaskStatus::Killed));
    }

    #[test]
    fn the_table_keeps_order_and_replaces_by_id() {
        let mut table = TaskTable::new();
        assert!(table.is_empty());
        table.start(shell("a"));
        table.start(shell("b"));
        let mut again = shell("a");
        again.description = "renamed".into();
        table.start(again);
        let snap = table.snapshot();
        let ids: Vec<&str> = snap.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids.len(), 2);
        assert_eq!(table.get("a").unwrap().description, "renamed");
        assert!(table.set_pid("a", 42));
        assert!(!table.set_pid("zz", 42));
        assert_eq!(table.get("a").unwrap().pid, Some(42));
        assert!(table.set_status("a", BackgroundTaskStatus::Killed));
        assert!(
            !table.set_status("a", BackgroundTaskStatus::Killed),
            "no change"
        );
        assert!(!table.set_status("zz", BackgroundTaskStatus::Killed));
    }

    #[test]
    fn a_report_of_the_cli_is_merged_without_losing_what_it_does_not_name() {
        let mut table = TaskTable::new();
        table.start(shell("toolu_bg"));
        table.set_pid("toolu_bg", 7);
        let reported = vec![
            // Named by tool call: the CLI's own id is another string.
            BackgroundTask {
                id: "b1".into(),
                kind: BackgroundTaskKind::Other,
                description: String::new(),
                status: BackgroundTaskStatus::Completed,
                started_at_ms: None,
                tool_call_id: Some("toolu_bg".into()),
                parent: None,
                pid: None,
            },
            // Unknown: added.
            BackgroundTask {
                id: "agent-1".into(),
                kind: BackgroundTaskKind::Agent,
                description: "explore".into(),
                status: BackgroundTaskStatus::Running,
                started_at_ms: Some(5),
                tool_call_id: None,
                parent: None,
                pid: None,
            },
        ];
        table.merge(reported);
        let snapshot = table.snapshot();
        assert_eq!(snapshot.len(), 2);
        let ours = table.get("toolu_bg").unwrap();
        assert_eq!(ours.status, BackgroundTaskStatus::Completed);
        assert_eq!(ours.kind, BackgroundTaskKind::Shell, "`other` says nothing");
        assert_eq!(ours.description, "nap", "an empty description says nothing");
        assert_eq!(ours.pid, Some(7), "the claimed pid survives");
        assert_eq!(
            table.get("agent-1").unwrap().kind,
            BackgroundTaskKind::Agent
        );
    }

    #[test]
    fn a_terminal_notification_ends_the_task_and_a_progress_one_does_not() {
        let mut table = TaskTable::new();
        table.start(shell("toolu_bg"));
        assert!(!table.apply_notification(Some("toolu_bg"), None, Some("running")));
        assert!(!table.apply_notification(Some("toolu_bg"), None, None));
        assert!(!table.apply_notification(Some("unknown"), None, Some("completed")));
        assert!(table.apply_notification(None, Some("toolu_bg"), Some("failed")));
        assert_eq!(
            table.get("toolu_bg").unwrap().status,
            BackgroundTaskStatus::Failed
        );
        assert!(
            !table.apply_notification(Some("toolu_bg"), None, Some("failed")),
            "already there: no change"
        );
    }

    #[test]
    fn killing_pids_marks_the_running_tasks_that_own_them() {
        let mut table = TaskTable::new();
        table.start(shell("a"));
        table.start(shell("b"));
        table.start(shell("c"));
        table.set_pid("a", 10);
        table.set_pid("b", 20);
        table.set_status("b", BackgroundTaskStatus::Completed);
        assert_eq!(table.mark_killed(&[10, 20, 30]), 1);
        assert_eq!(table.get("a").unwrap().status, BackgroundTaskStatus::Killed);
        assert_eq!(
            table.get("b").unwrap().status,
            BackgroundTaskStatus::Completed,
            "a finished task is not re-labelled"
        );
        assert_eq!(
            table.get("c").unwrap().status,
            BackgroundTaskStatus::Running
        );
        assert_eq!(table.mark_killed(&[10]), 0);
    }
}
