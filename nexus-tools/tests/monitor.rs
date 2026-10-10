//! `Monitor` end to end over the stdio transport (N20): lines arrive as notifications, in
//! order, and the stream ends with the command. The session ending kills a persistent
//! monitor and never hangs on it.
#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use nexus_tools::files::Scope;
use nexus_tools::shell::{ShellConfig, register};
use nexus_tools::{Profile, Server, Session, ToolRegistry, serve_lines};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct Wire {
    to_server: tokio::io::DuplexStream,
    from_server: BufReader<tokio::io::DuplexStream>,
    served: tokio::task::JoinHandle<()>,
    /// Notifications that arrived while a call was waiting for its answer.
    backlog: std::collections::VecDeque<Value>,
    /// The `nexus-tools/tasks` notifications (a task ended), kept apart from the monitor's lines.
    tasks: Vec<Value>,
    _dir: tempfile::TempDir,
}

fn start() -> Wire {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    let out = dir.path().join("out");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    let scope = Arc::new(Scope::new(&work, [out.clone()]).unwrap());
    let config = ShellConfig::new(scope, out).unwrap();
    let server = Server::new(register(ToolRegistry::new(), &config));
    let (to_server, server_in) = tokio::io::duplex(1 << 20);
    let (server_out, from_server) = tokio::io::duplex(1 << 20);
    let served = tokio::spawn(serve_lines(
        Arc::new(server),
        Session::new(Profile::unrestricted("s")),
        BufReader::new(server_in),
        server_out,
    ));
    Wire {
        to_server,
        from_server: BufReader::new(from_server),
        served,
        backlog: std::collections::VecDeque::new(),
        tasks: Vec::new(),
        _dir: dir,
    }
}

impl Wire {
    async fn send(&mut self, message: Value) {
        self.to_server
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap();
    }

    async fn read(&mut self) -> Value {
        if let Some(message) = self.backlog.pop_front() {
            return message;
        }
        self.read_wire().await
    }

    async fn read_raw(&mut self) -> Value {
        let mut line = String::new();
        tokio::time::timeout(
            Duration::from_secs(15),
            self.from_server.read_line(&mut line),
        )
        .await
        .expect("a message in time")
        .unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON ({e}): {line:?}"))
    }

    async fn read_wire(&mut self) -> Value {
        loop {
            let message = self.read_raw().await;
            if message["params"]["logger"] == "nexus-tools/tasks" {
                self.tasks.push(message);
                continue;
            }
            return message;
        }
    }

    /// The next `nexus-tools/tasks` notification (other messages read meanwhile are kept).
    async fn task_end(&mut self) -> Value {
        loop {
            if !self.tasks.is_empty() {
                return self.tasks.remove(0);
            }
            let message = self.read_raw().await;
            if message["params"]["logger"] == "nexus-tools/tasks" {
                return message;
            }
            self.backlog.push_back(message);
        }
    }

    /// Calls a tool and returns its answer plus every notification that came before it.
    async fn call(&mut self, id: u64, tool: &str, arguments: Value) -> Value {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":tool,"arguments":arguments}})).await;
        loop {
            let message = self.read_wire().await;
            if message["id"] == id {
                return message;
            }
            self.backlog.push_back(message);
        }
    }
}

fn data(message: &Value) -> &Value {
    assert_eq!(message["method"], "notifications/message", "{message}");
    assert_eq!(message["params"]["logger"], "nexus-tools/monitor");
    &message["params"]["data"]
}

#[tokio::test]
async fn each_line_is_a_notification_in_order_and_the_stream_ends_with_the_command() {
    let mut wire = start();
    let answer = wire
        .call(1, "Monitor", json!({"command": "printf 'a\\nb\\n'; sleep 0.4; printf 'c\\n'; echo oops 1>&2; printf 'no newline at the end'"}))
        .await;
    let text = answer["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("Monitor started with ID: "), "{text}");
    let id = text
        .split("ID: ")
        .nth(1)
        .unwrap()
        .split('.')
        .next()
        .unwrap()
        .to_owned();

    let mut lines = Vec::new();
    loop {
        let message = wire.read().await;
        let d = data(&message).clone();
        assert_eq!(d["task_id"], id);
        if d["event"] == "ended" {
            assert_eq!(d["status"], "exited with code 0");
            break;
        }
        lines.push(d["line"].as_str().unwrap().to_owned());
    }
    assert_eq!(lines, ["a", "b", "c", "oops", "no newline at the end"]);
}

#[tokio::test]
async fn task_stop_ends_a_monitor_and_the_end_is_reported() {
    let mut wire = start();
    let answer = wire
        .call(
            1,
            "Monitor",
            json!({"command": "echo up; sleep 300", "persistent": true}),
        )
        .await;
    let text = answer["result"]["content"][0]["text"].as_str().unwrap();
    let id = text
        .split("ID: ")
        .nth(1)
        .unwrap()
        .split('.')
        .next()
        .unwrap()
        .to_owned();
    let first = wire.read().await;
    assert_eq!(data(&first)["line"], "up");
    let stopped = wire.call(2, "TaskStop", json!({"task_id": id})).await;
    assert_eq!(stopped["result"]["isError"], false, "{stopped}");
    // The end notification may have arrived before the answer or arrive now.
    loop {
        let message = wire.read().await;
        if data(&message)["event"] == "ended" {
            assert_eq!(data(&message)["status"], "stopped");
            break;
        }
    }
    // And the harness's signal: one `nexus-tools/tasks` end, in the contract's vocabulary.
    let end = wire.task_end().await;
    assert_eq!(end["params"]["data"]["task_id"], json!(id), "{end}");
    assert_eq!(end["params"]["data"]["event"], "ended", "{end}");
    assert_eq!(end["params"]["data"]["status"], "killed", "{end}");
}

#[tokio::test]
async fn a_monitor_that_outlives_its_timeout_is_stopped() {
    let mut wire = start();
    wire.call(
        1,
        "Monitor",
        json!({"command": "sleep 300", "timeout_ms": 600}),
    )
    .await;
    let message = wire.read().await;
    assert_eq!(data(&message)["event"], "ended");
    assert_eq!(data(&message)["status"], "stopped");
}

/// A background task holds a clone of the notifier: it must not keep a session open after
/// its input ended (the first version of this transport hung exactly there).
#[tokio::test]
async fn closing_the_input_ends_the_session_even_with_a_persistent_monitor_running() {
    let mut wire = start();
    let pid_file = wire._dir.path().join("pid");
    wire.call(
        1,
        "Monitor",
        json!({"command": format!("echo $$ > {}; exec sleep 300", pid_file.display()), "persistent": true}),
    )
    .await;
    for _ in 0..100 {
        if pid_file.exists()
            && !std::fs::read_to_string(&pid_file)
                .unwrap()
                .trim()
                .is_empty()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let pid: u32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    drop(std::mem::replace(
        &mut wire.to_server,
        tokio::io::duplex(1).0,
    ));
    tokio::time::timeout(Duration::from_secs(10), &mut wire.served)
        .await
        .expect("serve_lines returns once its input is closed")
        .unwrap();
    // Dropping the server's session state kills the command; give the kill a moment.
    for _ in 0..100 {
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !alive {
            return;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    panic!("the monitored command outlived its session");
}
