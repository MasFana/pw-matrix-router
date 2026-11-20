//! Deadline-bound subprocesses with nonblocking pipes and process-group cancellation.

use super::{configure_pipe_size, nonblocking};
use anyhow::{Context, Result, bail};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const STDERR_LIMIT: usize = 4096;
const COMMAND_OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
const COMMAND_DEADLINE: Duration = Duration::from_millis(1500);
const IO_DEADLINE: Duration = Duration::from_secs(2);

pub(crate) fn command(program: &str) -> Command {
    let mut command = Command::new(program);
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
}

pub(crate) fn available(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            let path = dir.join(program);
            use std::os::unix::fs::PermissionsExt;
            path.metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
    })
}

fn poll(fd: RawFd, events: i16) -> io::Result<()> {
    let mut descriptor = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut descriptor, 1, 20) };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    Ok(())
}

fn retain_tail(buffer: &mut Vec<u8>, bytes: &[u8]) {
    if bytes.len() >= STDERR_LIMIT {
        buffer.clear();
        buffer.extend_from_slice(&bytes[bytes.len() - STDERR_LIMIT..]);
    } else {
        let excess = (buffer.len() + bytes.len()).saturating_sub(STDERR_LIMIT);
        if excess > 0 {
            buffer.drain(..excess);
        }
        buffer.extend_from_slice(bytes);
    }
}

pub(crate) struct AudioProcess {
    child: Child,
    input: Option<ChildStdin>,
    output: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    error_tail: Vec<u8>,
    status: Option<ExitStatus>,
}

impl AudioProcess {
    pub(crate) fn spawn(mut command: Command, capture: bool) -> Result<Self> {
        command
            .stdin(if capture {
                Stdio::null()
            } else {
                Stdio::piped()
            })
            .stdout(if capture {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stderr(Stdio::piped());
        let child = command
            .spawn()
            .with_context(|| format!("launch {}", command.get_program().to_string_lossy()))?;
        let mut process = Self {
            child,
            input: None,
            output: None,
            stderr: None,
            error_tail: Vec::with_capacity(STDERR_LIMIT),
            status: None,
        };
        process.input = process.child.stdin.take();
        process.output = process.child.stdout.take();
        process.stderr = process.child.stderr.take();
        if let Some(pipe) = &process.input {
            nonblocking(pipe)?;
            if let Err(error) = configure_pipe_size(pipe, 8192) {
                tracing::debug!(%error, "Playback pipe sizing unavailable");
            }
        }
        if let Some(pipe) = &process.output {
            nonblocking(pipe)?;
            if let Err(error) = configure_pipe_size(pipe, 16384) {
                tracing::debug!(%error, "Capture pipe sizing unavailable");
            }
        }
        if let Some(pipe) = &process.stderr {
            nonblocking(pipe)?;
        }
        Ok(process)
    }

    fn drain_stderr(&mut self) {
        let mut bytes = [0u8; 1024];
        if let Some(stderr) = &mut self.stderr {
            // Cap work per audio block even if a broken child writes endlessly.
            for _ in 0..8 {
                match stderr.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(count) => retain_tail(&mut self.error_tail, &bytes[..count]),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        }
    }

    pub(crate) fn error_text(&self) -> String {
        String::from_utf8_lossy(&self.error_tail).trim().to_owned()
    }

    pub(crate) fn read_block(&mut self, bytes: &mut [u8], stop: &AtomicBool) -> Result<bool> {
        let mut position = 0;
        let mut deadline = Instant::now() + IO_DEADLINE;
        while position < bytes.len() {
            if stop.load(Ordering::Acquire) {
                return Ok(false);
            }
            self.drain_stderr();
            let pipe = self.output.as_mut().context("capture stdout unavailable")?;
            match pipe.read(&mut bytes[position..]) {
                Ok(0) => bail!("capture stream closed"),
                Ok(count) => {
                    position += count;
                    deadline = Instant::now() + IO_DEADLINE;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    poll(pipe.as_raw_fd(), libc::POLLIN)?;
                }
                Err(error) => return Err(error).context("read capture stream"),
            }
            if Instant::now() >= deadline {
                bail!("capture stream stalled for 2 seconds");
            }
        }
        Ok(true)
    }

    pub(crate) fn write_block(&mut self, bytes: &[u8], stop: &AtomicBool) -> Result<bool> {
        let mut position = 0;
        let mut deadline = Instant::now() + IO_DEADLINE;
        while position < bytes.len() {
            if stop.load(Ordering::Acquire) {
                return Ok(false);
            }
            self.drain_stderr();
            let pipe = self.input.as_mut().context("playback stdin unavailable")?;
            match pipe.write(&bytes[position..]) {
                Ok(0) => bail!("playback stream closed"),
                Ok(count) => {
                    position += count;
                    deadline = Instant::now() + IO_DEADLINE;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    poll(pipe.as_raw_fd(), libc::POLLOUT)?;
                }
                Err(error) => return Err(error).context("write playback stream"),
            }
            if Instant::now() >= deadline {
                bail!("playback stream stalled for 2 seconds");
            }
        }
        if let Some(status) = self.child.try_wait()? {
            self.status = Some(status);
            bail!("playback process exited: {status}");
        }
        Ok(true)
    }

    pub(crate) fn terminate(&mut self) -> Result<()> {
        self.input.take();
        self.output.take();
        if self.status.is_none() {
            // Only this child's freshly created process group is ever signalled.
            unsafe {
                libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL);
            }
            let _ = self.child.kill();
            self.status = Some(self.child.wait().context("reap audio child")?);
        }
        self.drain_stderr();
        self.stderr.take();
        Ok(())
    }
}

impl Drop for AudioProcess {
    fn drop(&mut self) {
        if let Err(error) = self.terminate() {
            tracing::error!(%error, "Audio child cleanup failed");
        }
    }
}

pub(crate) fn pactl(args: &[&str], stop: Option<&AtomicBool>) -> Result<Vec<u8>> {
    let mut command = command("pactl");
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command
        .spawn()
        .with_context(|| format!("launch pactl {}", args.join(" ")))?;
    let mut process = AudioProcess {
        child,
        input: None,
        output: None,
        stderr: None,
        error_tail: Vec::with_capacity(STDERR_LIMIT),
        status: None,
    };
    process.output = process.child.stdout.take();
    process.stderr = process.child.stderr.take();
    if let Some(pipe) = &process.output {
        nonblocking(pipe)?;
    }
    if let Some(pipe) = &process.stderr {
        nonblocking(pipe)?;
    }
    let mut output = Vec::new();
    let mut bytes = [0u8; 8192];
    let deadline = Instant::now() + COMMAND_DEADLINE;
    loop {
        process.drain_stderr();
        if let Some(pipe) = &mut process.output {
            for _ in 0..32 {
                match pipe.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(count) => {
                        if output.len() + count > COMMAND_OUTPUT_LIMIT {
                            bail!("pactl output exceeded 8 MiB");
                        }
                        output.extend_from_slice(&bytes[..count]);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => return Err(error).context("read pactl output"),
                }
            }
        }
        if let Some(status) = process.child.try_wait()? {
            process.status = Some(status);
            // On exit drain the final output before closing its pipe.
            if let Some(pipe) = &mut process.output {
                loop {
                    match pipe.read(&mut bytes) {
                        Ok(0) => break,
                        Ok(count) => {
                            if output.len() + count > COMMAND_OUTPUT_LIMIT {
                                bail!("pactl output exceeded 8 MiB");
                            }
                            output.extend_from_slice(&bytes[..count]);
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            }
            process.drain_stderr();
            if !status.success() {
                bail!(
                    "pactl {} failed ({status}): {}",
                    args.join(" "),
                    process.error_text()
                );
            }
            return Ok(output);
        }
        if stop.is_some_and(|stop| stop.load(Ordering::Acquire)) {
            bail!("pactl {} cancelled", args.join(" "));
        }
        if Instant::now() >= deadline {
            bail!(
                "pactl {} timed out after 1500ms: {}",
                args.join(" "),
                process.error_text()
            );
        }
        if let Some(pipe) = &process.output {
            poll(pipe.as_raw_fd(), libc::POLLIN)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stderr_tail_is_bounded_and_keeps_latest_error() {
        let mut bytes = Vec::new();
        retain_tail(&mut bytes, &vec![b'x'; STDERR_LIMIT * 2]);
        retain_tail(&mut bytes, b"last error");
        assert_eq!(bytes.len(), STDERR_LIMIT);
        assert!(bytes.ends_with(b"last error"));
    }
}
