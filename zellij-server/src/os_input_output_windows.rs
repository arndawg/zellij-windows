use crate::os_input_output::{command_exists, AsyncReader};
use crate::panes::PaneId;

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Read, Write},
    sync::{Arc, Mutex},
    thread,
    time::Instant,
};

use zellij_utils::{errors::prelude::*, input::command::RunCommand};

pub use async_trait::async_trait;

/// Spawn a short-lived helper process that delivers CTRL_C_EVENT to the console
/// that `child_pid` is attached to (the ConPTY virtual console).
///
/// The helper is a DETACHED_PROCESS (no console of its own) so it can freely
/// call `AttachConsole(child_pid)` without affecting the server.  It then calls
/// `GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0)` and exits.
fn spawn_ctrl_c_helper(child_pid: u32) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x00000008;

    match std::env::current_exe() {
        Ok(exe) => {
            if let Err(e) = std::process::Command::new(exe)
                .args(["--_send-ctrl-c", &child_pid.to_string()])
                .creation_flags(DETACHED_PROCESS)
                .spawn()
            {
                log::warn!("Failed to spawn Ctrl-C helper for pid {}: {}", child_pid, e);
            }
        },
        Err(e) => {
            log::warn!("Failed to get current exe path for Ctrl-C helper: {}", e);
        },
    }
}

/// Wraps a `portable-pty` reader, bridging blocking I/O to async via a channel.
///
/// A background thread reads from the PTY master in a loop and sends chunks
/// through a `tokio::sync::mpsc` channel. The `AsyncReader::read()` impl
/// awaits on the channel receiver.
struct WindowsAsyncReader {
    rx: tokio::sync::mpsc::Receiver<io::Result<Vec<u8>>>,
    pending: Vec<u8>,
}

impl WindowsAsyncReader {
    fn new(mut reader: Box<dyn Read + Send>) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        thread::Builder::new()
            .name("pty_reader".to_string())
            .spawn(move || {
                let mut buf = vec![0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => {
                            // EOF
                            break;
                        },
                        Ok(n) => {
                            if tx.blocking_send(Ok(buf[..n].to_vec())).is_err() {
                                break; // receiver dropped
                            }
                        },
                        Err(e) => {
                            let _ = tx.blocking_send(Err(e));
                            break;
                        },
                    }
                }
            })
            .expect("failed to spawn pty_reader thread");
        Self {
            rx,
            pending: Vec::new(),
        }
    }
}

#[async_trait]
impl AsyncReader for WindowsAsyncReader {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, io::Error> {
        // Drain any pending data first
        if !self.pending.is_empty() {
            let n = std::cmp::min(buf.len(), self.pending.len());
            buf[..n].copy_from_slice(&self.pending[..n]);
            self.pending.drain(..n);
            return Ok(n);
        }
        match self.rx.recv().await {
            Some(Ok(data)) => {
                let n = std::cmp::min(buf.len(), data.len());
                buf[..n].copy_from_slice(&data[..n]);
                if n < data.len() {
                    self.pending.extend_from_slice(&data[n..]);
                }
                Ok(n)
            },
            Some(Err(e)) => Err(e),
            None => Ok(0), // channel closed = EOF
        }
    }
}

/// Holds the master side of a PTY plus ancillary handles.
struct MasterHandle {
    master: Box<dyn MasterPty + Send>,
    writer: Option<Box<dyn Write + Send>>,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    child_pid: u32,
    /// Timestamp of the last Ctrl+C (byte 0x03) write for escalation logic.
    /// First Ctrl+C: only write byte 0x03 (lets raw-mode programs handle it).
    /// Second Ctrl+C within 1 second: escalate to CTRL_BREAK_EVENT to force-
    /// interrupt programs that don't read stdin (e.g. `dir /s`).
    last_ctrl_c: Option<Instant>,
}

/// The Windows PTY backend. Uses `portable-pty` (ConPTY) under the hood.
#[derive(Clone)]
pub(crate) struct WindowsPtyBackend {
    terminal_id_to_master: Arc<Mutex<BTreeMap<u32, Option<MasterHandle>>>>,
}

impl WindowsPtyBackend {
    pub fn new() -> Result<Self, io::Error> {
        Ok(Self {
            terminal_id_to_master: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    pub fn spawn_terminal(
        &self,
        cmd: RunCommand,
        failover_cmd: Option<RunCommand>,
        quit_cb: Box<dyn Fn(PaneId, Option<i32>, RunCommand) + Send>,
        terminal_id: u32,
    ) -> Result<(Box<dyn AsyncReader>, u32)> {
        let err_context = |cmd: &RunCommand| {
            format!(
                "failed to spawn terminal for command '{}'",
                cmd.command.to_string_lossy()
            )
        };

        if !command_exists(&cmd) {
            if let Some(failover) = failover_cmd {
                return self.spawn_terminal(failover, None, quit_cb, terminal_id);
            }
            return Err(ZellijError::CommandNotFound {
                terminal_id,
                command: cmd.command.to_string_lossy().to_string(),
            })
            .with_context(|| err_context(&cmd));
        }

        let pty_system = native_pty_system();

        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| anyhow::anyhow!("failed to open pty: {}", e))
            .with_context(|| err_context(&cmd))?;

        let mut cmd_builder = CommandBuilder::new(&cmd.command);
        cmd_builder.args(&cmd.args);
        if let Some(cwd) = &cmd.cwd {
            if cwd.exists() && cwd.is_dir() {
                cmd_builder.cwd(cwd);
            } else {
                log::error!(
                    "Failed to set CWD for new pane. '{}' does not exist or is not a folder",
                    cwd.display()
                );
            }
        }
        cmd_builder.env("ZELLIJ_PANE_ID", format!("{}", terminal_id));

        let mut child = pair
            .slave
            .spawn_command(cmd_builder)
            .map_err(|e| anyhow::anyhow!("failed to spawn command: {}", e))
            .with_context(|| err_context(&cmd))?;

        let child_pid = child
            .process_id()
            .unwrap_or(0);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| anyhow::anyhow!("failed to clone pty reader: {}", e))
            .with_context(|| err_context(&cmd))?;

        let mut writer = pair
            .master
            .take_writer()
            .map_err(|e| anyhow::anyhow!("failed to take pty writer: {}", e))
            .with_context(|| err_context(&cmd))?;

        // ConPTY sends a Device Status Report (ESC[6n) on startup and blocks
        // all child output until it receives a cursor position response.
        // Pre-emptively send the response so the child can start immediately
        // rather than waiting for the query to flow through the full pipeline.
        let _ = writer.write_all(b"\x1b[1;1R");
        let _ = writer.flush();

        let killer = child.clone_killer();

        let handle = MasterHandle {
            master: pair.master,
            writer: Some(writer),
            killer,
            child_pid,
            last_ctrl_c: None,
        };

        self.terminal_id_to_master
            .lock()
            .to_anyhow()
            .with_context(|| err_context(&cmd))?
            .insert(terminal_id, Some(handle));

        // Spawn a thread to wait for child exit and invoke the quit callback
        let cmd_for_cb = cmd.clone();
        thread::Builder::new()
            .name(format!("pty_wait_{}", terminal_id))
            .spawn(move || {
                let exit_status = child.wait();
                let exit_code = match exit_status {
                    Ok(status) => {
                        if status.success() {
                            Some(0)
                        } else {
                            // portable-pty ExitStatus doesn't expose the raw code on all
                            // platforms, so we report non-zero generically
                            Some(1)
                        }
                    },
                    Err(e) => {
                        log::error!("Error waiting for child process: {}", e);
                        None
                    },
                };
                quit_cb(PaneId::Terminal(terminal_id), exit_code, cmd_for_cb);
            })
            .with_context(|| err_context(&cmd))?;

        let async_reader = Box::new(WindowsAsyncReader::new(reader)) as Box<dyn AsyncReader>;
        Ok((async_reader, child_pid as u32))
    }

    pub fn set_terminal_size(
        &self,
        terminal_id: u32,
        cols: u16,
        rows: u16,
        _width_in_pixels: Option<u16>,
        _height_in_pixels: Option<u16>,
    ) -> Result<()> {
        let err_context = || {
            format!(
                "failed to set terminal id {} to size ({}, {})",
                terminal_id, rows, cols
            )
        };

        let mut map = self
            .terminal_id_to_master
            .lock()
            .to_anyhow()
            .with_context(err_context)?;

        match map.get_mut(&terminal_id) {
            Some(Some(handle)) => {
                if cols > 0 && rows > 0 {
                    handle
                        .master
                        .resize(PtySize {
                            rows,
                            cols,
                            pixel_width: 0,
                            pixel_height: 0,
                        })
                        .map_err(|e| anyhow::anyhow!("resize failed: {}", e))
                        .with_context(err_context)?;
                }
            },
            _ => {
                Err::<(), _>(anyhow!("failed to find terminal for id {terminal_id}"))
                    .with_context(err_context)
                    .non_fatal();
            },
        }
        Ok(())
    }

    pub fn write_to_tty_stdin(&self, terminal_id: u32, buf: &[u8]) -> Result<usize> {
        let err_context = || format!("failed to write to stdin of TTY ID {}", terminal_id);

        let mut map = self
            .terminal_id_to_master
            .lock()
            .to_anyhow()
            .with_context(err_context)?;

        match map.get_mut(&terminal_id) {
            Some(Some(handle)) => {
                // Escalating Ctrl+C for ConPTY:
                //
                // Writing raw byte 0x03 to the ConPTY pipe triggers
                // ENABLE_PROCESSED_INPUT handling: the console subsystem
                // generates a real CTRL_C_EVENT signal that kills the child
                // process immediately — before raw-mode programs (crossterm-
                // based apps like Claude Code, vim) can intercept it.
                //
                // Instead, we send a Win32 input mode escape sequence that
                // ConPTY translates into an INPUT_RECORD placed in the
                // child's console input buffer.  Raw-mode programs read it
                // as a Ctrl+C key event; programs with ENABLE_PROCESSED_INPUT
                // see it as a ^C at the prompt without being killed.
                //
                // Strategy:
                //   1st Ctrl+C: send Win32 input mode key event (safe for
                //     interactive programs — they handle it gracefully)
                //   2nd Ctrl+C within 1s: send CTRL_BREAK_EVENT via helper
                //     process to force-interrupt unresponsive commands
                //     (e.g. `dir /s c:\`)
                if buf == [0x03] {
                    // Escalation temporarily disabled for testing.
                    // let now = Instant::now();
                    // let should_escalate = handle
                    //     .last_ctrl_c
                    //     .map(|prev| now.duration_since(prev).as_millis() < 1000)
                    //     .unwrap_or(false);
                    // if should_escalate {
                    //     spawn_ctrl_c_helper(handle.child_pid);
                    //     handle.last_ctrl_c = None;
                    // } else {
                    //     handle.last_ctrl_c = Some(now);
                    // }

                    // Win32 input mode escape sequence for Ctrl+C key-down:
                    //   \x1b[Vk;Sc;Uc;Kd;Cs;Rc_
                    //   Vk=67 (VK_C), Sc=46 (scan code), Uc=3 (ETX),
                    //   Kd=1 (key down), Cs=8 (LEFT_CTRL_PRESSED), Rc=1
                    const CTRL_C_KEY_EVENT: &[u8] = b"\x1b[67;46;3;1;8;1_";
                    if let Some(writer) = handle.writer.as_mut() {
                        return writer
                            .write(CTRL_C_KEY_EVENT)
                            .map_err(|e| anyhow::anyhow!("{}", e))
                            .with_context(err_context);
                    } else {
                        return Err(anyhow!("writer not available"))
                            .with_context(err_context);
                    }
                }

                if let Some(writer) = handle.writer.as_mut() {
                    writer
                        .write(buf)
                        .map_err(|e| anyhow::anyhow!("{}", e))
                        .with_context(err_context)
                } else {
                    Err(anyhow!("writer not available")).with_context(err_context)
                }
            },
            _ => Err(anyhow!("could not find terminal handle")).with_context(err_context),
        }
    }

    pub fn tcdrain(&self, terminal_id: u32) -> Result<()> {
        let err_context = || format!("failed to tcdrain to TTY ID {}", terminal_id);

        let mut map = self
            .terminal_id_to_master
            .lock()
            .to_anyhow()
            .with_context(err_context)?;

        match map.get_mut(&terminal_id) {
            Some(Some(handle)) => {
                if let Some(writer) = handle.writer.as_mut() {
                    writer
                        .flush()
                        .map_err(|e| anyhow::anyhow!("{}", e))
                        .with_context(err_context)
                } else {
                    Ok(())
                }
            },
            _ => Err(anyhow!("could not find terminal handle")).with_context(err_context),
        }
    }

    pub fn kill(&self, pid: u32) -> Result<()> {
        let mut map = self.terminal_id_to_master.lock().to_anyhow()?;
        for handle_opt in map.values_mut() {
            if let Some(handle) = handle_opt {
                if handle.child_pid == pid {
                    let _ = handle.killer.kill();
                    return Ok(());
                }
            }
        }
        // Fallback: use TerminateProcess directly
        use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
        unsafe {
            let proc_handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !proc_handle.is_null() {
                TerminateProcess(proc_handle, 1);
                windows_sys::Win32::Foundation::CloseHandle(proc_handle);
            }
        }
        Ok(())
    }

    pub fn force_kill(&self, pid: u32) -> Result<()> {
        // On Windows, TerminateProcess is already forceful
        self.kill(pid)
    }

    pub fn send_sigint(&self, pid: u32) -> Result<()> {
        // Use the helper-process approach to deliver CTRL_C_EVENT to the
        // child's ConPTY console without disturbing the server's own console.
        spawn_ctrl_c_helper(pid);
        Ok(())
    }

    pub fn reserve_terminal_id(&self, terminal_id: u32) {
        self.terminal_id_to_master
            .lock()
            .unwrap()
            .insert(terminal_id, None);
    }

    pub fn clear_terminal_id(&self, terminal_id: u32) {
        self.terminal_id_to_master
            .lock()
            .unwrap()
            .remove(&terminal_id);
    }

    pub fn next_terminal_id(&self) -> Option<u32> {
        self.terminal_id_to_master
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect::<BTreeSet<u32>>()
            .last()
            .map(|l| l + 1)
            .or(Some(0))
    }
}
