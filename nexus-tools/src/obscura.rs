//! Obscura embedded (`obscura` feature, decision 1e9d7501): the headless browser of
//! `obscura/` runs **inside this binary**, with no external program.
//!
//! Two uses, one discipline:
//! - the keyless `obscura` search engine ([`crate::search::EmbeddedObscuraBackend`]): each query
//!   runs on a browser thread of its own ([`run_isolated`]);
//! - the browser tools: `nexus-tools obscura mcp` serves Obscura's `browser_*` tools over MCP
//!   stdio ([`serve_mcp`]), what the harness attaches as its `browser` server.
//!
//! What this module guarantees:
//! - **A thread of its own.** A V8 isolate belongs to the thread that made it, so browser work
//!   runs on a dedicated thread with a single-threaded runtime, never on the server's.
//! - **Stopped, not abandoned.** When the caller gives up (the request is cancelled, the time
//!   limit passes), every isolate that thread made is told to stop its JavaScript at once
//!   (`IsolateHandle::terminate_execution`, thread-safe) and the work's future is dropped at its
//!   next await: its network requests go with it. A page stuck in a synchronous loop is ended the
//!   same way, not after Obscura's own 5 s watchdog.
//! - **Bounded.** The same limits as the external engine (nexus #96): 30 s per query or call,
//!   2 MiB of answer.
//! - **Private networks stay off.** No stealth flag, no private-network flag, and the browser
//!   refuses to start when `OBSCURA_ALLOW_PRIVATE_NETWORK` asks for them in this process's
//!   environment ([`private_network_refusal`]).

use std::collections::VecDeque;
use std::future::Future;
use std::io::{BufRead, Write};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use obscura_js::runtime::IsolateHandle;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

/// How long one browser call or one search may take (the external engine's limit, #96).
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// The most a browser answer may weigh (the external engine's limit, #96).
pub const MAX_ANSWER_BYTES: usize = 2 * 1024 * 1024;

static LIVE_THREADS: AtomicUsize = AtomicUsize::new(0);

/// Browser threads alive now. A stopped call leaves none behind (tests read it).
pub fn live_browser_threads() -> usize {
    LIVE_THREADS.load(Ordering::SeqCst)
}

struct LiveThread;

impl LiveThread {
    fn enter() -> Self {
        LIVE_THREADS.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for LiveThread {
    fn drop(&mut self) {
        LIVE_THREADS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Why the embedded browser cannot run here, if it cannot: `OBSCURA_ALLOW_PRIVATE_NETWORK`
/// set to a true value in this process's environment would let pages reach the machine and its
/// network (Obscura reads it process-wide), and nexus never runs the browser that way.
pub fn private_network_refusal() -> Option<String> {
    private_network_refusal_for(
        std::env::var("OBSCURA_ALLOW_PRIVATE_NETWORK")
            .ok()
            .as_deref(),
    )
}

/// [`private_network_refusal`] for a given value of the variable (Obscura's own reading of it).
pub fn private_network_refusal_for(value: Option<&str>) -> Option<String> {
    let value = value?.trim().to_ascii_lowercase();
    matches!(value.as_str(), "1" | "true" | "yes" | "on").then(|| {
        "OBSCURA_ALLOW_PRIVATE_NETWORK is set in nexus-tools' environment: the embedded browser never runs with private networks allowed; unset it".to_owned()
    })
}

/// The isolates a browser thread has made, so another thread can end their JavaScript.
#[derive(Clone, Default)]
struct Isolates(Arc<Mutex<Vec<IsolateHandle>>>);

impl Isolates {
    /// Records every isolate the current thread makes from now on.
    fn watch_this_thread(&self) {
        let isolates = self.clone();
        obscura_js::runtime::set_isolate_observer(move |handle| {
            isolates
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(handle.clone());
        });
    }

    /// Ends the JavaScript running in each of them now. An isolate already disposed ignores it.
    fn terminate_all(&self) {
        for handle in self.0.lock().unwrap_or_else(PoisonError::into_inner).iter() {
            handle.terminate_execution();
        }
    }
}

/// Why browser work gave no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stopped {
    /// The time limit passed; the work was stopped.
    TimedOut,
    /// The thread could not start, or the work ended without an answer (a panic).
    Failed(String),
}

/// Stops the work when dropped, unless disarmed: the future of a cancelled call is dropped, and
/// so is the browser work it started.
struct StopOnDrop {
    isolates: Isolates,
    stop: Option<oneshot::Sender<()>>,
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            self.isolates.terminate_all();
            let _ = stop.send(());
        }
    }
}

/// Runs `work` on a browser thread of its own and waits at most `limit` for its answer. The
/// work is built ON that thread (`work()`), so it may hold the browser, which is not `Send`.
///
/// Dropping the returned future (a cancelled request), or the limit passing, stops the work:
/// its isolates are terminated and its future dropped. The thread then ends by itself.
pub async fn run_isolated<W, F, T>(work: W, limit: Duration) -> Result<T, Stopped>
where
    W: FnOnce() -> F + Send + 'static,
    F: Future<Output = T> + 'static,
    T: Send + 'static,
{
    let isolates = Isolates::default();
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let (done_tx, done_rx) = oneshot::channel::<T>();
    let watched = isolates.clone();
    // The guard exists before the thread does: a call cancelled while the thread starts still
    // stops it.
    let mut guard = StopOnDrop {
        isolates,
        stop: Some(stop_tx),
    };
    std::thread::Builder::new()
        .name("nexus-obscura".into())
        .spawn(move || {
            let _live = LiveThread::enter();
            watched.watch_this_thread();
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                obscura_js::runtime::clear_isolate_observer();
                return;
            };
            let answer = runtime.block_on(async move {
                tokio::select! {
                    biased;
                    _ = stop_rx => None,
                    answer = work() => Some(answer),
                }
            });
            // Tasks the work spawned (fetches, timers) end with the runtime.
            drop(runtime);
            obscura_js::runtime::clear_isolate_observer();
            if let Some(answer) = answer {
                let _ = done_tx.send(answer);
            }
        })
        .map_err(|e| Stopped::Failed(format!("the browser thread did not start: {e}")))?;
    match tokio::time::timeout(limit, done_rx).await {
        Ok(Ok(answer)) => {
            guard.stop = None;
            Ok(answer)
        },
        Ok(Err(_)) => Err(Stopped::Failed(
            "the browser stopped without an answer".to_owned(),
        )),
        Err(_) => Err(Stopped::TimedOut),
    }
}

// ---------------------------------------------------------------------------
// `nexus-tools obscura …`
// ---------------------------------------------------------------------------

const OBSCURA_USAGE: &str = "usage: nexus-tools obscura mcp [--proxy URL] [--user-agent UA] [--call-timeout-secs N]\n       nexus-tools obscura serp [--max-results N] [--] QUERY";

/// `nexus-tools obscura <command>`: the embedded browser without the tool server around it.
/// `args` are the arguments after `obscura`.
pub fn main(args: Vec<String>) -> ExitCode {
    if let Some(why) = private_network_refusal() {
        eprintln!("nexus-tools: {why}");
        return ExitCode::from(2);
    }
    let mut args = args.into_iter();
    match args.next().as_deref() {
        Some("mcp") => match McpOptions::parse(args) {
            Ok(options) => serve_mcp(options),
            Err(message) => fail(&message),
        },
        Some("serp") => serp(args),
        Some("--help" | "-h") => {
            println!("{OBSCURA_USAGE}");
            ExitCode::SUCCESS
        },
        _ => fail(OBSCURA_USAGE),
    }
}

fn fail(message: &str) -> ExitCode {
    eprintln!("nexus-tools: {message}");
    ExitCode::from(2)
}

/// `serp`: one search, printed as Obscura's own `serp` prints it (a JSON array of
/// `{url, title, snippet}`). A manual check of the real engine, outside CI.
fn serp(args: impl Iterator<Item = String>) -> ExitCode {
    let mut max_results = 10usize;
    let mut words = Vec::new();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--max-results" => match args.next().and_then(|v| v.parse().ok()) {
                Some(n) => max_results = n,
                None => return fail("--max-results takes a number"),
            },
            "--" => words.extend(args.by_ref()),
            _ => words.push(arg),
        }
    }
    let query = words.join(" ");
    if query.trim().is_empty() {
        return fail(OBSCURA_USAGE);
    }
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return fail("cannot start the runtime");
    };
    let answer = runtime.block_on(run_isolated(
        move || async move {
            let mut state = obscura_mcp::BrowserState::new(None, None, false);
            obscura_mcp::serp_search(&mut state, &query, "duckduckgo", max_results).await
        },
        CALL_TIMEOUT,
    ));
    match answer {
        Ok(Ok(results)) => match serde_json::to_string_pretty(&results) {
            Ok(text) => {
                println!("{text}");
                ExitCode::SUCCESS
            },
            Err(e) => fail(&e.to_string()),
        },
        Ok(Err(why)) => fail(&why),
        Err(Stopped::TimedOut) => fail("the search did not finish in time"),
        Err(Stopped::Failed(why)) => fail(&why),
    }
}

/// How `nexus-tools obscura mcp` runs.
#[derive(Debug, Clone)]
pub struct McpOptions {
    proxy: Option<String>,
    user_agent: Option<String>,
    call_timeout: Duration,
}

impl McpOptions {
    /// Reads the arguments after `mcp`. `--stealth` and `--allow-private-network` are refused:
    /// nexus never switches them on (N23).
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut options = Self {
            proxy: None,
            user_agent: None,
            call_timeout: CALL_TIMEOUT,
        };
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
            match arg.as_str() {
                "--proxy" => options.proxy = Some(value("--proxy")?),
                "--user-agent" => options.user_agent = Some(value("--user-agent")?),
                "--call-timeout-secs" => {
                    let secs: u64 = value("--call-timeout-secs")?
                        .parse()
                        .map_err(|_| "--call-timeout-secs takes a number of seconds")?;
                    options.call_timeout = Duration::from_secs(secs.max(1));
                },
                "--stealth" | "--allow-private-network" => {
                    return Err(format!(
                        "the browser is never started with `{arg}`: stealth mode and private-network access are not switched on by nexus"
                    ));
                },
                other => return Err(format!("unknown argument: {other}\n{OBSCURA_USAGE}")),
            }
        }
        Ok(options)
    }
}

/// What the transport hands the browser thread.
enum Inbound {
    /// A request (it has an id), and its parsed message.
    Request(Value),
    /// `notifications/cancelled` for this request id.
    Cancel(Value),
}

/// What the browser thread has read and not yet run.
struct Inbox {
    rx: mpsc::UnboundedReceiver<Inbound>,
    /// Requests read while a call ran: answered in order, before anything read later.
    backlog: VecDeque<Inbound>,
    /// Requests cancelled before they ran: skipped when their turn comes.
    cancelled_early: Vec<Value>,
}

/// The request the browser thread is answering now, so the reader can stop it at once.
#[derive(Default)]
struct InFlight {
    id: Option<Value>,
    cancelled: bool,
}

/// `nexus-tools obscura mcp`: Obscura's `browser_*` tools over MCP stdio, one browser for the
/// life of the process (tabs, cookies and pages persist between calls, as with `obscura mcp`).
///
/// Unlike Obscura's own loop, every `tools/call` here is bounded and cancellable:
/// `notifications/cancelled` naming the running request terminates the page's JavaScript and
/// drops the call (no answer is sent, as MCP asks); a call past its time limit is stopped the
/// same way and answered with an error result; an answer over 2 MiB is replaced by an error.
pub fn serve_mcp(options: McpOptions) -> ExitCode {
    let isolates = Isolates::default();
    let in_flight = Arc::new(Mutex::new(InFlight::default()));
    let (tx, rx) = mpsc::unbounded_channel::<Inbound>();

    // The reader: a plain thread, so it can stop a call while the browser thread is inside V8.
    {
        let isolates = isolates.clone();
        let in_flight = in_flight.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("nexus-obscura-stdin".into())
            .spawn(move || read_stdin(tx, &isolates, &in_flight))
        {
            return fail(&format!("cannot read the input: {e}"));
        }
    }

    let _live = LiveThread::enter();
    isolates.watch_this_thread();
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return fail("cannot start the runtime");
    };
    let code = runtime.block_on(browser_loop(options, rx, &isolates, &in_flight));
    obscura_js::runtime::clear_isolate_observer();
    code
}

fn read_stdin(
    tx: mpsc::UnboundedSender<Inbound>,
    isolates: &Isolates,
    in_flight: &Mutex<InFlight>,
) {
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if message.get("method").and_then(Value::as_str) == Some("notifications/cancelled") {
            let Some(id) = message.pointer("/params/requestId").cloned() else {
                continue;
            };
            {
                let mut running = in_flight.lock().unwrap_or_else(PoisonError::into_inner);
                if running.id.as_ref() == Some(&id) {
                    running.cancelled = true;
                    isolates.terminate_all();
                }
            }
            if tx.send(Inbound::Cancel(id)).is_err() {
                break;
            }
            continue;
        }
        // Other notifications need no answer.
        if message.get("id").is_none_or(Value::is_null) {
            continue;
        }
        if tx.send(Inbound::Request(message)).is_err() {
            break;
        }
    }
}

async fn browser_loop(
    options: McpOptions,
    rx: mpsc::UnboundedReceiver<Inbound>,
    isolates: &Isolates,
    in_flight: &Mutex<InFlight>,
) -> ExitCode {
    let mut state = obscura_mcp::BrowserState::new(options.proxy, options.user_agent, false);
    let mut pump_armed = false;
    let mut inbox = Inbox {
        rx,
        backlog: VecDeque::new(),
        cancelled_early: Vec::new(),
    };
    loop {
        // Between calls the active page keeps running (timers, fetches), as in `obscura mcp`.
        let inbound = if let Some(next) = inbox.backlog.pop_front() {
            Some(next)
        } else if pump_armed {
            tokio::select! {
                biased;
                inbound = inbox.rx.recv() => inbound,
                pumped = state.advance_active_page_tasks() => {
                    pump_armed = matches!(pumped, Ok(false));
                    continue;
                }
            }
        } else {
            inbox.rx.recv().await
        };
        let Some(inbound) = inbound else {
            return ExitCode::SUCCESS;
        };
        let message = match inbound {
            Inbound::Request(message) => message,
            Inbound::Cancel(id) => {
                // A cancel for a request still queued: it will not be run.
                inbox.cancelled_early.push(id);
                continue;
            },
        };
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        if let Some(at) = inbox.cancelled_early.iter().position(|c| *c == id) {
            inbox.cancelled_early.swap_remove(at);
            continue;
        }
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        *in_flight.lock().unwrap_or_else(PoisonError::into_inner) = InFlight {
            id: Some(id.clone()),
            cancelled: false,
        };
        let answer = answer_one(
            &method,
            id.clone(),
            &params,
            &mut state,
            &mut inbox,
            isolates,
            options.call_timeout,
        )
        .await;
        let was_cancelled =
            std::mem::take(&mut *in_flight.lock().unwrap_or_else(PoisonError::into_inner))
                .cancelled;
        // A stopped call left its isolate terminating: clear that before the next call (a no-op
        // otherwise). After the reset above, the reader can no longer stop this call.
        state.clear_termination();
        let line = match answer {
            Answer::Done(_) | Answer::TimedOut if was_cancelled => None,
            Answer::Cancelled => None,
            Answer::Done(response) => Some(capped(response, &id)),
            Answer::TimedOut => Some(error_result(
                &id,
                &format!(
                    "the browser did not finish in {} s; the call was stopped",
                    options.call_timeout.as_secs()
                ),
            )),
        };
        if let Some(line) = line {
            let mut out = std::io::stdout().lock();
            if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
                return ExitCode::SUCCESS;
            }
        }
        pump_armed = state.has_active_page_runtime();
    }
}

enum Answer {
    Done(Value),
    Cancelled,
    TimedOut,
}

async fn answer_one(
    method: &str,
    id: Value,
    params: &Value,
    state: &mut obscura_mcp::BrowserState,
    inbox: &mut Inbox,
    isolates: &Isolates,
    limit: Duration,
) -> Answer {
    // The time limit is enforced from another thread too: this one may be inside V8, where no
    // await point lets the deadline below fire.
    let watchdog = Watchdog::arm(isolates.clone(), limit);
    let call = obscura_mcp::dispatch(method, id.clone(), params, state);
    tokio::pin!(call);
    let deadline = tokio::time::sleep(limit);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            biased;
            inbound = inbox.rx.recv() => match inbound {
                Some(Inbound::Cancel(cancel)) if cancel == id => break Answer::Cancelled,
                Some(Inbound::Cancel(other)) => inbox.cancelled_early.push(other),
                // Requests are answered in order: one waits for the one before it.
                Some(request @ Inbound::Request(_)) => inbox.backlog.push_back(request),
                None => break Answer::Cancelled,
            },
            () = &mut deadline => break Answer::TimedOut,
            // A call the watchdog stopped may still return (V8 unwound, the page went on):
            // it is a timeout all the same.
            response = &mut call => break if watchdog.fired() {
                Answer::TimedOut
            } else {
                Answer::Done(serde_json::to_value(&response).unwrap_or(Value::Null))
            },
        }
    }
}

fn error_result(id: &Value, text: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {"content": [{"type": "text", "text": text}], "isError": true},
    })
    .to_string()
}

/// The response line, or an error result when it is over [`MAX_ANSWER_BYTES`].
fn capped(response: Value, id: &Value) -> String {
    let line = response.to_string();
    if line.len() > MAX_ANSWER_BYTES {
        error_result(
            id,
            &format!(
                "the browser's answer is larger than {} KiB and was not returned; ask for less (a selector, max_chars)",
                MAX_ANSWER_BYTES / 1024
            ),
        )
    } else {
        line
    }
}

/// Terminates the isolates when `limit` passes, unless dropped first. A thread, because the
/// browser thread itself may be stuck in a synchronous script.
struct Watchdog {
    fired: Arc<std::sync::atomic::AtomicBool>,
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watchdog {
    fn arm(isolates: Isolates, limit: Duration) -> Self {
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = fired.clone();
        let thread = std::thread::Builder::new()
            .name("nexus-obscura-watchdog".into())
            .spawn(move || {
                if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(limit)
                {
                    flag.store(true, Ordering::SeqCst);
                    isolates.terminate_all();
                }
            })
            .ok();
        Self {
            fired,
            stop: Some(stop),
            thread,
        }
    }

    fn fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
