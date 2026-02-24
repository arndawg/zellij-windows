use crate::os_input_output::{command_exists, AsyncReader};
use crate::panes::PaneId;

use portable_pty::{CommandBuilder, MasterPty, PtySize};

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Read, Write},
    sync::{Arc, Mutex},
    thread,
};

use zellij_utils::{errors::prelude::*, input::command::RunCommand};

pub use async_trait::async_trait;

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

        // Use ConPtySystem directly for the large (1MB) output pipe buffer
        // which reduces conhost lock contention during heavy output.
        use portable_pty::win::conpty::ConPtySystem;
        use portable_pty::PtySystem;
        let pty_system = ConPtySystem::default();

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
                if buf == [0x03] {
                    // Ctrl+C handling for Windows ConPTY. Three mechanisms
                    // are used because no single one works in all cases:
                    //
                    // 1. Write byte 0x03 to the ConPTY pipe — works when the
                    //    shell is at an idle prompt (ReadConsoleW processes it).
                    //
                    // 2. Terminate descendant processes — works for external
                    //    commands (ping, node, cargo, etc.) that are child
                    //    processes of the shell.
                    //
                    // 3. Send a win32-input-mode encoded Ctrl+Break event —
                    //    works for built-in commands (dir /s, etc.) that run
                    //    inside the shell process itself with no children.
                    //    The CSI sequence is always parsed by conhost's VT
                    //    input thread regardless of CreatePseudoConsole flags.
                    if let Some(writer) = handle.writer.as_mut() {
                        let _ = writer.write_all(b"\x03");
                        let _ = writer.flush();
                    }
                    let shell_pid = handle.child_pid;
                    // Drop the lock before the potentially slow process walk
                    drop(map);
                    let had_descendants = Self::terminate_descendants(shell_pid);

                    if !had_descendants {
                        // No child processes found — likely a built-in command.
                        // Send CTRL_BREAK_EVENT via win32-input-mode VT sequence.
                        // Format: CSI Vk;Sc;Uc;Kd;Cs;Rc _
                        //   VK_CANCEL=3, Sc=70, Uc=0, Kd=1/0, Cs=8(LEFT_CTRL), Rc=1
                        let mut map = self
                            .terminal_id_to_master
                            .lock()
                            .to_anyhow()
                            .with_context(err_context)?;
                        if let Some(Some(handle)) = map.get_mut(&terminal_id) {
                            if let Some(writer) = handle.writer.as_mut() {
                                let _ = writer.write_all(b"\x1b[3;70;0;1;8;1_");
                                let _ = writer.write_all(b"\x1b[3;70;0;0;8;1_");
                                let _ = writer.flush();
                            }
                        }
                    }
                    return Ok(1);
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
        // Terminate descendant processes of the shell. This is the Windows
        // equivalent of sending SIGINT: GenerateConsoleCtrlEvent is broken
        // in ConPTY sessions, so we directly terminate the child processes.
        Self::terminate_descendants(pid);
        Ok(())
    }

    /// Terminate all descendant processes of `parent_pid` without killing
    /// `parent_pid` itself (the shell). Uses the Toolhelp API to walk the
    /// process tree and find children, then recursively terminates them
    /// bottom-up (leaves first).
    /// Returns `true` if any descendants were found and terminated.
    fn terminate_descendants(parent_pid: u32) -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, TerminateProcess, PROCESS_TERMINATE,
        };

        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                log::error!("CreateToolhelp32Snapshot failed");
                return false;
            }

            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

            // Collect all processes and their parent PIDs
            let mut all_procs: Vec<(u32, u32)> = Vec::new(); // (pid, parent_pid)
            if Process32FirstW(snapshot, &mut entry) != 0 {
                loop {
                    all_procs.push((entry.th32ProcessID, entry.th32ParentProcessID));
                    if Process32NextW(snapshot, &mut entry) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snapshot);

            // Find all descendants of parent_pid using BFS
            let mut descendants: Vec<u32> = Vec::new();
            let mut queue: Vec<u32> = vec![parent_pid];
            while let Some(pid) = queue.pop() {
                for &(child_pid, ppid) in &all_procs {
                    if ppid == pid && child_pid != parent_pid {
                        descendants.push(child_pid);
                        queue.push(child_pid);
                    }
                }
            }

            if descendants.is_empty() {
                return false;
            }

            log::info!(
                "Ctrl+C: terminating {} descendants of shell PID {}: {:?}",
                descendants.len(),
                parent_pid,
                descendants
            );

            // Terminate in reverse order (leaves first)
            for &pid in descendants.iter().rev() {
                let proc_handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
                if !proc_handle.is_null() && proc_handle != INVALID_HANDLE_VALUE {
                    TerminateProcess(proc_handle, 1);
                    CloseHandle(proc_handle);
                }
            }
            true
        }
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
