use super::*;

fn make_server() -> ServerOsInputOutput {
    get_server_os_input().expect("failed to create server os input")
}

#[test]
fn get_cwd() {
    let server = make_server();

    let pid = std::process::id();
    assert!(
        server.get_cwd(pid).is_some(),
        "Get current working directory from PID {}",
        pid
    );
}

// --- Signal delivery tests (Unix only) ---

#[cfg(not(windows))]
#[test]
fn kill_sends_sighup_to_process() {
    let child = Command::new("sleep")
        .arg("60")
        .spawn()
        .expect("failed to spawn sleep");
    let pid = child.id();

    let server = make_server();

    server.kill(pid).expect("kill should succeed");

    // Give the signal time to be delivered
    std::thread::sleep(std::time::Duration::from_millis(100));
}

#[cfg(not(windows))]
#[test]
fn force_kill_sends_sigkill_to_process() {
    let child = Command::new("sleep")
        .arg("60")
        .spawn()
        .expect("failed to spawn sleep");
    let pid = child.id();

    let server = make_server();

    server.force_kill(pid).expect("force_kill should succeed");

    std::thread::sleep(std::time::Duration::from_millis(100));
}

#[cfg(not(windows))]
#[test]
fn send_sigint_to_process() {
    let child = Command::new("cat")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn cat");
    let pid = child.id();

    let server = make_server();

    server.send_sigint(pid).expect("send_sigint should succeed");

    std::thread::sleep(std::time::Duration::from_millis(100));
}

// --- Terminal ID management tests (cross-platform) ---

#[test]
fn next_terminal_id_starts_at_zero() {
    let backend = PtyBackendImpl::new().expect("failed to create backend");
    let id = backend.next_terminal_id();
    assert_eq!(id, Some(0), "first terminal ID should be 0");
}

#[test]
fn next_terminal_id_increments_after_reserve() {
    let backend = PtyBackendImpl::new().expect("failed to create backend");

    let id0 = backend.next_terminal_id().unwrap();
    backend.reserve_terminal_id(id0);

    let id1 = backend.next_terminal_id().unwrap();
    assert!(id1 > id0, "next ID ({}) should be greater than reserved ({})", id1, id0);

    backend.reserve_terminal_id(id1);

    let id2 = backend.next_terminal_id().unwrap();
    assert!(id2 > id1, "next ID ({}) should be greater than reserved ({})", id2, id1);
}

#[test]
fn clear_terminal_id_removes_entry() {
    let backend = PtyBackendImpl::new().expect("failed to create backend");

    backend.reserve_terminal_id(0);
    backend.reserve_terminal_id(1);
    assert_eq!(backend.next_terminal_id(), Some(2));

    backend.clear_terminal_id(1);
    // After clearing ID 1, the max key is 0, so next should be 1
    assert_eq!(backend.next_terminal_id(), Some(1));
}

#[test]
fn reserve_and_clear_all_returns_to_zero() {
    let backend = PtyBackendImpl::new().expect("failed to create backend");

    backend.reserve_terminal_id(0);
    backend.reserve_terminal_id(1);
    backend.clear_terminal_id(0);
    backend.clear_terminal_id(1);

    assert_eq!(backend.next_terminal_id(), Some(0));
}

// --- Windows PTY backend tests ---

#[cfg(windows)]
mod windows_pty_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn make_cmd(command: &str, args: &[&str]) -> RunCommand {
        RunCommand {
            command: PathBuf::from(command),
            args: args.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn noop_quit_cb() -> Box<dyn Fn(PaneId, Option<i32>, RunCommand) + Send> {
        Box::new(|_, _, _| {})
    }

    #[test]
    fn spawn_terminal_returns_valid_pid() {
        let backend = PtyBackendImpl::new().expect("failed to create backend");
        backend.reserve_terminal_id(0);

        let cmd = make_cmd("cmd.exe", &["/C", "echo hello"]);
        let (_reader, pid) = backend
            .spawn_terminal(cmd, None, noop_quit_cb(), 0)
            .expect("spawn_terminal should succeed");

        assert!(pid > 0, "child PID should be positive, got {}", pid);
    }

    #[test]
    fn spawn_terminal_nonexistent_command_errors() {
        let backend = PtyBackendImpl::new().expect("failed to create backend");
        backend.reserve_terminal_id(0);

        let cmd = make_cmd("this_command_does_not_exist_12345", &[]);
        let result = backend.spawn_terminal(cmd, None, noop_quit_cb(), 0);

        assert!(result.is_err(), "spawning a nonexistent command should fail");
    }

    #[test]
    fn spawn_terminal_with_failover() {
        let backend = PtyBackendImpl::new().expect("failed to create backend");
        backend.reserve_terminal_id(0);

        let bad_cmd = make_cmd("this_command_does_not_exist_12345", &[]);
        let good_cmd = make_cmd("cmd.exe", &["/C", "echo failover"]);

        let result = backend.spawn_terminal(bad_cmd, Some(good_cmd), noop_quit_cb(), 0);
        assert!(result.is_ok(), "should fall back to failover command");
    }

    #[tokio::test]
    async fn spawn_terminal_runs_command_and_produces_output() {
        let backend = PtyBackendImpl::new().expect("failed to create backend");
        backend.reserve_terminal_id(0);

        let cmd = make_cmd("cmd.exe", &["/C", "echo hello_from_pty"]);
        let (mut reader, _pid) = backend
            .spawn_terminal(cmd, None, noop_quit_cb(), 0)
            .expect("spawn_terminal should succeed");

        let mut all_output = Vec::new();
        let mut buf = vec![0u8; 4096];
        let mut dsr_responded = false;
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(10);

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, reader.read(&mut buf)).await {
                Ok(Ok(0)) => break,  // EOF
                Ok(Ok(n)) => {
                    all_output.extend_from_slice(&buf[..n]);

                    // ConPTY sends a Device Status Report (ESC[6n) during init and
                    // blocks further output until the "terminal" responds with a
                    // cursor position report.  Write one back to unblock it.
                    if !dsr_responded
                        && all_output.windows(4).any(|w| w == b"\x1b[6n")
                    {
                        dsr_responded = true;
                        let _ = backend.write_to_tty_stdin(0, b"\x1b[1;1R");
                    }

                    // Stop early once we have what we need
                    let output = String::from_utf8_lossy(&all_output);
                    if output.contains("hello_from_pty") {
                        break;
                    }
                },
                Ok(Err(_)) => break,
                Err(_) => break, // timeout
            }
        }

        let output = String::from_utf8_lossy(&all_output);
        assert!(
            output.contains("hello_from_pty"),
            "expected output to contain 'hello_from_pty', got: {:?}",
            output
        );
    }

    #[tokio::test]
    async fn async_reader_returns_eof_on_child_exit() {
        let quit_called = Arc::new(Mutex::new(false));
        let quit_called_clone = quit_called.clone();

        let backend = PtyBackendImpl::new().expect("failed to create backend");
        backend.reserve_terminal_id(0);

        // This command exits immediately
        let cmd = make_cmd("cmd.exe", &["/C", "echo done"]);
        let (mut reader, _pid) = backend
            .spawn_terminal(
                cmd,
                None,
                Box::new(move |_, _, _| {
                    *quit_called_clone.lock().unwrap() = true;
                }),
                0,
            )
            .expect("spawn_terminal should succeed");

        // Phase 1: Read output and respond to DSR queries to unblock ConPTY.
        // ConPTY sends a Device Status Report (ESC[6n) during init and blocks
        // the entire terminal session until it gets a cursor position response.
        // Without this, cmd.exe can't write its output and can't exit.
        let mut all_output = Vec::new();
        let mut buf = vec![0u8; 4096];
        let mut dsr_responded = false;
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(10);

        loop {
            if *quit_called.lock().unwrap() {
                break;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                panic!("child did not exit within timeout");
            }
            match tokio::time::timeout(
                tokio::time::Duration::from_millis(200),
                reader.read(&mut buf),
            )
            .await
            {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => {
                    all_output.extend_from_slice(&buf[..n]);
                    if !dsr_responded
                        && all_output.windows(4).any(|w| w == b"\x1b[6n")
                    {
                        dsr_responded = true;
                        let _ = backend.write_to_tty_stdin(0, b"\x1b[1;1R");
                    }
                },
                Ok(Err(_)) => break,
                Err(_) => continue, // short timeout -- keep polling quit_called
            }
        }

        // Phase 2: ConPTY keeps the read pipe open even after the child exits.
        // Drop the master handle by clearing the terminal ID -- this closes
        // the ConPTY, which causes the reader thread to get an error/EOF.
        backend.clear_terminal_id(0);

        // Phase 3: Drain remaining data and wait for EOF.
        let mut saw_eof = false;
        let eof_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);

        loop {
            let remaining = eof_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, reader.read(&mut buf)).await {
                Ok(Ok(0)) => {
                    saw_eof = true;
                    break;
                },
                Ok(Ok(_)) => continue, // drain remaining output
                Ok(Err(_)) => {
                    // Error from closed pipe counts as EOF for our purposes
                    saw_eof = true;
                    break;
                },
                Err(_) => break, // timeout
            }
        }

        assert!(saw_eof, "should receive EOF or error after master handle is dropped");
    }

    #[test]
    fn set_terminal_size_succeeds() {
        let backend = PtyBackendImpl::new().expect("failed to create backend");
        backend.reserve_terminal_id(0);

        let cmd = make_cmd("cmd.exe", &["/C", "timeout /T 5 >nul"]);
        let (_reader, _pid) = backend
            .spawn_terminal(cmd, None, noop_quit_cb(), 0)
            .expect("spawn_terminal should succeed");

        let result = backend.set_terminal_size(0, 120, 40, None, None);
        assert!(result.is_ok(), "set_terminal_size should succeed: {:?}", result.err());
    }

    #[test]
    fn write_to_tty_stdin_succeeds() {
        let backend = PtyBackendImpl::new().expect("failed to create backend");
        backend.reserve_terminal_id(0);

        let cmd = make_cmd("cmd.exe", &["/C", "timeout /T 5 >nul"]);
        let (_reader, _pid) = backend
            .spawn_terminal(cmd, None, noop_quit_cb(), 0)
            .expect("spawn_terminal should succeed");

        let result = backend.write_to_tty_stdin(0, b"hello\r\n");
        assert!(result.is_ok(), "write_to_tty_stdin should succeed: {:?}", result.err());
    }

    #[test]
    fn tcdrain_succeeds() {
        let backend = PtyBackendImpl::new().expect("failed to create backend");
        backend.reserve_terminal_id(0);

        let cmd = make_cmd("cmd.exe", &["/C", "timeout /T 5 >nul"]);
        let (_reader, _pid) = backend
            .spawn_terminal(cmd, None, noop_quit_cb(), 0)
            .expect("spawn_terminal should succeed");

        let result = backend.tcdrain(0);
        assert!(result.is_ok(), "tcdrain should succeed: {:?}", result.err());
    }

    #[test]
    fn kill_terminates_spawned_process() {
        let quit_called = Arc::new(Mutex::new(false));
        let quit_called_clone = quit_called.clone();

        let backend = PtyBackendImpl::new().expect("failed to create backend");
        backend.reserve_terminal_id(0);

        // Use a command that runs long enough to be killed
        let cmd = make_cmd("cmd.exe", &["/C", "timeout /T 60 >nul"]);
        let (_reader, pid) = backend
            .spawn_terminal(
                cmd,
                None,
                Box::new(move |_, _, _| {
                    *quit_called_clone.lock().unwrap() = true;
                }),
                0,
            )
            .expect("spawn_terminal should succeed");

        let result = backend.kill(pid);
        assert!(result.is_ok(), "kill should succeed: {:?}", result.err());

        // Wait for the quit callback to fire
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if *quit_called.lock().unwrap() {
                break;
            }
        }
        assert!(*quit_called.lock().unwrap(), "quit callback should have been invoked");
    }

    #[test]
    fn force_kill_terminates_spawned_process() {
        let quit_called = Arc::new(Mutex::new(false));
        let quit_called_clone = quit_called.clone();

        let backend = PtyBackendImpl::new().expect("failed to create backend");
        backend.reserve_terminal_id(0);

        let cmd = make_cmd("cmd.exe", &["/C", "timeout /T 60 >nul"]);
        let (_reader, pid) = backend
            .spawn_terminal(
                cmd,
                None,
                Box::new(move |_, _, _| {
                    *quit_called_clone.lock().unwrap() = true;
                }),
                0,
            )
            .expect("spawn_terminal should succeed");

        let result = backend.force_kill(pid);
        assert!(result.is_ok(), "force_kill should succeed: {:?}", result.err());

        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if *quit_called.lock().unwrap() {
                break;
            }
        }
        assert!(*quit_called.lock().unwrap(), "quit callback should have been invoked");
    }
}
