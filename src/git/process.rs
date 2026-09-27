use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::thread;
use std::time::Duration;

pub const GIT_READ_CANCELLED: &str = "Git read cancelled";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadError {
    Cancelled,
    Diagnostic(String),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str(GIT_READ_CANCELLED),
            Self::Diagnostic(message) => formatter.write_str(message),
        }
    }
}

thread_local! {
    static GIT_READ_CANCELLATION: RefCell<Option<(Arc<AtomicU64>, u64)>> = const {
        RefCell::new(None)
    };
}

#[cfg(test)]
thread_local! {
    static GIT_PROCESSES_STARTED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn git_processes_started() -> usize {
    GIT_PROCESSES_STARTED.with(std::cell::Cell::get)
}

fn record_process_start() {
    #[cfg(test)]
    GIT_PROCESSES_STARTED.with(|count| count.set(count.get() + 1));
}

pub fn with_read_cancellation<T>(
    generation: Arc<AtomicU64>,
    expected: u64,
    work: impl FnOnce() -> T,
) -> T {
    GIT_READ_CANCELLATION.with(|slot| {
        let previous = slot.replace(Some((generation, expected)));
        let result = work();
        slot.replace(previous);
        result
    })
}

pub fn with_read_cancellation_result<T>(
    generation: Arc<AtomicU64>,
    expected: u64,
    work: impl FnOnce() -> Result<T, String>,
) -> Result<T, ReadError> {
    let observed_generation = Arc::clone(&generation);
    let result = with_read_cancellation(generation, expected, work);
    if observed_generation.load(Ordering::Acquire) != expected {
        Err(ReadError::Cancelled)
    } else {
        result.map_err(ReadError::Diagnostic)
    }
}

pub(super) fn read_cancelled() -> bool {
    GIT_READ_CANCELLATION.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|(generation, expected)| generation.load(Ordering::Acquire) != *expected)
    })
}

pub(super) fn git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = git_output(cwd, args)?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub(super) fn git_difference(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = git_output(cwd, args)?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() || (output.status.code() == Some(1) && !stdout.is_empty()) {
        Ok(stdout)
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

pub(super) fn git_owned_output(cwd: &Path, args: &[String]) -> Result<Output, String> {
    let mut command = git_command(cwd);
    command.args(args);
    cancellable_command_output(command)
}

pub(super) fn git_rebase(cwd: &Path, args: &[String], state: &Path) -> Result<Output, String> {
    let mut command = git_command(cwd);
    command
        .args(args)
        .env("HERDR_REBASE_DIR", state)
        .env(
            "GIT_SEQUENCE_EDITOR",
            "sh \"$HERDR_REBASE_DIR/sequence-editor\"",
        )
        .env("GIT_EDITOR", "true");
    cancellable_command_output(command)
}

fn git_output(cwd: &Path, args: &[&str]) -> Result<Output, String> {
    let mut command = git_command(cwd);
    command.args(args);
    cancellable_command_output(command)
}

pub(super) fn git_command(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(cwd)
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null());
    command
}

pub(super) fn cancellable_command_output(mut command: Command) -> Result<Output, String> {
    record_process_start();
    let cancellation = GIT_READ_CANCELLATION.with(|slot| slot.borrow().clone());
    let Some((generation, expected)) = cancellation else {
        return command
            .output()
            .map_err(|error| format!("could not run Git: {error}"));
    };
    if generation.load(Ordering::Acquire) != expected {
        return Err(GIT_READ_CANCELLED.to_owned());
    }

    let mut child = command
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not run Git: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "could not capture Git stdout".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "could not capture Git stderr".to_owned())?;
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut stream = stdout;
        stream.read_to_end(&mut bytes).map(|_| bytes)
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut stream = stderr;
        stream.read_to_end(&mut bytes).map(|_| bytes)
    });

    let status = loop {
        if generation.load(Ordering::Acquire) != expected {
            terminate_child(&mut child);
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(GIT_READ_CANCELLED.to_owned());
        }
        match child
            .try_wait()
            .map_err(|error| format!("could not wait for Git: {error}"))?
        {
            Some(status) => break status,
            None => thread::sleep(Duration::from_millis(10)),
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| "Git stdout reader stopped".to_owned())?
        .map_err(|error| format!("could not read Git stdout: {error}"))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| "Git stderr reader stopped".to_owned())?
        .map_err(|error| format!("could not read Git stderr: {error}"))?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

pub(super) struct GitLines {
    child: std::process::Child,
    lines: Option<std::sync::mpsc::Receiver<Result<String, String>>>,
    stdout: Option<thread::JoinHandle<()>>,
    stderr: Option<thread::JoinHandle<Vec<u8>>>,
    finished: bool,
}

impl GitLines {
    pub(super) fn start(root: &Path, args: &[&str]) -> Result<Self, String> {
        if read_cancelled() {
            return Err(GIT_READ_CANCELLED.to_owned());
        }
        let mut child = git_command(root)
            .args(args)
            .process_group(0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Could not start history: {e}"))?;
        record_process_start();
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let stdout = thread::spawn(move || {
            for line in BufReader::new(stdout).split(b'\n') {
                let line = line
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .map_err(|e| e.to_string());
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr = thread::spawn(move || {
            let mut stream = stderr;
            let mut bytes = Vec::new();
            let _ = stream.by_ref().take(65536).read_to_end(&mut bytes);
            let _ = std::io::copy(&mut stream, &mut std::io::sink());
            bytes
        });
        Ok(Self {
            child,
            lines: Some(rx),
            stdout: Some(stdout),
            stderr: Some(stderr),
            finished: false,
        })
    }

    #[cfg(test)]
    pub(super) fn pid(&self) -> u32 {
        self.child.id()
    }

    pub(super) fn next(&mut self) -> Result<Option<String>, String> {
        if self.finished {
            return Ok(None);
        }
        loop {
            if read_cancelled() {
                return Err(GIT_READ_CANCELLED.to_owned());
            }
            match self
                .lines
                .as_ref()
                .expect("open stream")
                .recv_timeout(Duration::from_millis(10))
            {
                Ok(line) => return line.map(Some),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let status = loop {
            if read_cancelled() {
                return Err(GIT_READ_CANCELLED.to_owned());
            }
            if let Some(status) = self.child.try_wait().map_err(|e| e.to_string())? {
                break status;
            }
            thread::sleep(Duration::from_millis(10));
        };
        self.finished = true;
        let error = self
            .stderr
            .take()
            .expect("stderr reader")
            .join()
            .unwrap_or_default();
        if !status.success() {
            return Err(String::from_utf8_lossy(&error).trim().to_owned());
        }
        Ok(None)
    }
}

impl Drop for GitLines {
    fn drop(&mut self) {
        self.lines.take();
        if !self.finished {
            terminate_child(&mut self.child);
        }
        if let Some(reader) = self.stdout.take() {
            let _ = reader.join();
        }
        if let Some(reader) = self.stderr.take() {
            let _ = reader.join();
        }
    }
}

fn terminate_child(child: &mut std::process::Child) {
    let pid = child.id() as libc::pid_t;
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
    }
    for _ in 0..20 {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    let _ = child.wait();
}
