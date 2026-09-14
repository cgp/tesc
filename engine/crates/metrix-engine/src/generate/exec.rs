//! The escape hatch: a script that already exists, in a process of its own.
//!
//! No longer the recommended default (design-engine §7.2) — Lua covers the same need
//! without the IPC cost or the second deployable — but retained, because "we already
//! have a Python script that builds these payloads" is a real situation and rewriting
//! it in Lua to run a load test is not a reasonable ask.
//!
//! **`ndjson` against a pool of long-lived processes.** One JSON object in per
//! request, one out, newline delimited. The pool is started before the arrival clock
//! does: a sidecar that forked its first process on the first arrival would charge
//! that fork to the first request's latency.
//!
//! **`oneshot` forks per call**, which is honest about its cost rather than hiding
//! it: it is rate-capped so a plan cannot accidentally fork ten thousand processes a
//! second, and the run is annotated so nobody reads its latency as the service's.
//!
//! A generator that needs to write, execute, or reach the network wants this tier,
//! where the process boundary makes the cost and the risk explicit.

use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use metrix_plan::ExecProtocol;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use super::{Built, Context, require};

/// `oneshot` forks a process per request. Above this many a second the plan is doing
/// something it will not be able to sustain, and saying so beats melting the box.
const ONESHOT_CEILING: f64 = 200.0;

/// One declared exec generator.
pub(crate) struct Sidecar {
    /// The program and its arguments, the program resolved inside the bundle when it
    /// is a path within it.
    command: Vec<String>,
    directory: std::path::PathBuf,
    protocol: ExecProtocol,
    timeout: Duration,
    /// The long-lived processes, taken one at a time. A `Mutex` per process rather
    /// than one over the pool: two workers with two processes must not wait on each
    /// other, and a process speaking a request-per-line protocol can only serve one
    /// caller at a time.
    workers: Vec<Mutex<Option<Worker>>>,
    /// Round robin over the pool, so a slow call does not pin every caller to one
    /// process while the rest idle.
    next: AtomicU64,
    /// `oneshot` forks so far, for the ceiling.
    forks: AtomicU64,
    started: std::time::Instant,
}

struct Worker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Sidecar {
    pub fn prepare(
        at: &str,
        root: &Path,
        command: &[String],
        protocol: ExecProtocol,
        pool: Option<u32>,
        timeout_ms: Option<u64>,
    ) -> Result<Self, String> {
        require(
            !command.is_empty() && !command[0].is_empty(),
            &format!("{at}/command: an empty command"),
        )?;
        let pool = pool.unwrap_or(4).max(1) as usize;
        require(
            pool <= 256,
            &format!("{at}/pool: {pool} processes is more than a load box should fork"),
        )?;
        let timeout = Duration::from_millis(timeout_ms.unwrap_or(1000));
        require(
            !timeout.is_zero(),
            &format!("{at}/timeout_ms: must be positive"),
        )?;
        // A pool is meaningless when every call is its own process.
        let workers = match protocol {
            ExecProtocol::Ndjson => (0..pool).map(|_| Mutex::new(None)).collect(),
            ExecProtocol::Oneshot => Vec::new(),
        };
        Ok(Self {
            command: command.to_vec(),
            directory: root.to_path_buf(),
            protocol,
            timeout,
            workers,
            next: AtomicU64::new(0),
            forks: AtomicU64::new(0),
            started: std::time::Instant::now(),
        })
    }

    /// Fork the pool before the run starts.
    pub async fn start(&self) -> Result<(), String> {
        for slot in &self.workers {
            let mut held = slot.lock().await;
            *held = Some(self.spawn().await?);
        }
        Ok(())
    }

    fn spawn_command(&self) -> Command {
        let mut command = Command::new(&self.command[0]);
        command
            .args(&self.command[1..])
            .current_dir(&self.directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Inherited, so a sidecar's own diagnostics reach the operator's terminal
            // rather than filling a pipe nobody drains until the process blocks.
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        command
    }

    async fn spawn(&self) -> Result<Worker, String> {
        let mut child = self
            .spawn_command()
            .spawn()
            .map_err(|error| format!("cannot start {:?} — {error}", self.command[0]))?;
        let stdin = child.stdin.take().expect("a piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("a piped stdout"));
        Ok(Worker {
            child,
            stdin,
            stdout,
        })
    }

    pub async fn call(&self, context: &mut Context<'_>) -> Result<Built, String> {
        let line = serde_json::to_string(&payload(context)).map_err(|e| e.to_string())?;
        let answer = match self.protocol {
            ExecProtocol::Ndjson => self.ask_pool(&line).await,
            ExecProtocol::Oneshot => self.ask_fresh(&line).await,
        }?;
        parse(&answer)
    }

    /// One round trip against a process from the pool.
    ///
    /// A process that failed the exchange is dropped rather than reused: it may have
    /// half a line buffered, and the next caller would read this caller's answer.
    async fn ask_pool(&self, line: &str) -> Result<String, String> {
        let index = self.next.fetch_add(1, Ordering::Relaxed) as usize % self.workers.len();
        let mut slot = self.workers[index].lock().await;
        if slot.is_none() {
            *slot = Some(self.spawn().await?);
        }
        let worker = slot.as_mut().expect("just spawned");
        match tokio::time::timeout(self.timeout, exchange(worker, line)).await {
            Ok(Ok(answer)) => Ok(answer),
            Ok(Err(error)) => {
                *slot = None;
                Err(error)
            }
            Err(_) => {
                *slot = None;
                Err(format!(
                    "the sidecar did not answer within {}ms",
                    self.timeout.as_millis()
                ))
            }
        }
    }

    /// A process per call, capped.
    async fn ask_fresh(&self, line: &str) -> Result<String, String> {
        let forks = self.forks.fetch_add(1, Ordering::Relaxed) + 1;
        let elapsed = self.started.elapsed().as_secs_f64().max(0.001);
        require(
            forks as f64 / elapsed <= ONESHOT_CEILING,
            &format!(
                "the oneshot generator is forking more than {ONESHOT_CEILING:.0} processes a \
                 second; use the ndjson protocol, whose pool is the same script without the \
                 fork"
            ),
        )?;
        let mut worker = self.spawn().await?;
        let answer = tokio::time::timeout(self.timeout, exchange(&mut worker, line)).await;
        // The process has answered its one question; nothing waits for its exit.
        let _ = worker.child.start_kill();
        match answer {
            Ok(result) => result,
            Err(_) => Err(format!(
                "the sidecar did not answer within {}ms",
                self.timeout.as_millis()
            )),
        }
    }
}

/// One line in, one line out.
async fn exchange(worker: &mut Worker, line: &str) -> Result<String, String> {
    worker
        .stdin
        .write_all(line.as_bytes())
        .await
        .map_err(|error| format!("cannot write to the sidecar — {error}"))?;
    worker
        .stdin
        .write_all(b"\n")
        .await
        .map_err(|error| format!("cannot write to the sidecar — {error}"))?;
    worker
        .stdin
        .flush()
        .await
        .map_err(|error| format!("cannot write to the sidecar — {error}"))?;
    let mut answer = String::new();
    let read = worker
        .stdout
        .read_line(&mut answer)
        .await
        .map_err(|error| format!("cannot read from the sidecar — {error}"))?;
    require(read > 0, "the sidecar closed its output")?;
    Ok(answer)
}

/// The context as the object the sidecar receives.
fn payload(context: &mut Context<'_>) -> Value {
    let rows: Map<String, Value> = context
        .rows
        .iter()
        .map(|(dataset, fields)| {
            let row: Map<String, Value> = fields
                .iter()
                .map(|(column, value)| ((*column).to_owned(), json!(value)))
                .collect();
            ((*dataset).to_owned(), Value::Object(row))
        })
        .collect();
    json!({
        "vu": context.vu,
        "iteration": context.iteration,
        "step": context.step,
        "vars": context.vars,
        "rows": rows,
        "args": context.args,
        // A number rather than a callback: there is no way to call back across a pipe
        // without a second round trip per draw, and the seed is what makes it replay.
        "rng": context.rng.next_u64(),
    })
}

/// What the sidecar returned, as the parts of a request.
#[derive(Deserialize)]
struct Answer {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    query: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default)]
    headers: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default)]
    body: Option<String>,
    /// A sidecar saying it could not build this one. Its own error class, not the
    /// service's.
    #[serde(default)]
    error: Option<String>,
}

fn parse(line: &str) -> Result<Built, String> {
    let answer: Answer = serde_json::from_str(line.trim()).map_err(|error| {
        format!("the sidecar returned something that is not a request — {error}")
    })?;
    if let Some(error) = answer.error {
        return Err(error);
    }
    Ok(Built {
        path: answer.path,
        query: answer.query.map(|map| map.into_iter().collect()),
        headers: answer.headers.map(|map| map.into_iter().collect()),
        body: answer.body.map(String::into_bytes),
    })
}
