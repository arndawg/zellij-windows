//! Integration tests that exercise the release binary.
//!
//! These tests invoke `zellij.exe` / `zellij` as a subprocess and verify that
//! subcommands that touch real runtime paths (IPC, session discovery, config
//! parsing) work correctly — not just `--version` which only tests CLI parsing.
//!
//! The `list_sessions_with_no_sessions_exits_cleanly` test is the key regression
//! test: it exercises the exact code path (path_to_ipc_name -> IPC connect) that
//! previously panicked on Windows with "not a named pipe path".

use std::process::Command;

/// Returns the path to the zellij binary built by cargo.
fn zellij_bin() -> std::path::PathBuf {
    // cargo places the binary in the same deps dir as the test binary
    let mut path = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    if cfg!(windows) {
        path.push("zellij.exe");
    } else {
        path.push("zellij");
    }
    if !path.exists() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        path = std::path::PathBuf::from(manifest_dir)
            .join("target")
            .join(profile);
        if cfg!(windows) {
            path.push("zellij.exe");
        } else {
            path.push("zellij");
        }
    }
    assert!(
        path.exists(),
        "zellij binary not found at {:?} — build it first with `cargo build`",
        path
    );
    path
}

/// Helper: run zellij with args and return (stdout, stderr, exit_code).
fn run_zellij(args: &[&str]) -> (String, String, i32) {
    let output = Command::new(zellij_bin())
        .args(args)
        .output()
        .expect("failed to execute zellij");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let code = output.status.code().unwrap_or(-1);
    (stdout, stderr, code)
}

/// Helper: assert that stderr does not contain a panic message.
fn assert_no_panic(stderr: &str, context: &str) {
    assert!(
        !stderr.contains("panicked at"),
        "{}: binary panicked!\nstderr: {}",
        context,
        stderr
    );
    assert!(
        !stderr.contains("RUST_BACKTRACE"),
        "{}: binary panicked (backtrace hint found)!\nstderr: {}",
        context,
        stderr
    );
}

// ─── Basic subcommand tests ───────────────────────────────────────────────

#[test]
fn version_prints_version_string() {
    let (stdout, stderr, code) = run_zellij(&["--version"]);
    assert_no_panic(&stderr, "--version");
    assert_eq!(code, 0);
    assert!(
        stdout.contains("zellij"),
        "expected version output, got: {}",
        stdout
    );
}

#[test]
fn help_lists_subcommands() {
    let (stdout, stderr, code) = run_zellij(&["--help"]);
    assert_no_panic(&stderr, "--help");
    assert_eq!(code, 0);
    for subcmd in &["setup", "list-sessions", "attach", "kill-session"] {
        assert!(
            stdout.contains(subcmd),
            "--help output missing '{}', got:\n{}",
            subcmd,
            stdout
        );
    }
}

// ─── Setup subcommands (exercise config parsing, path resolution) ─────────

#[test]
fn setup_check_runs_without_panic() {
    let (stdout, stderr, code) = run_zellij(&["setup", "--check"]);
    assert_no_panic(&stderr, "setup --check");
    assert_eq!(code, 0, "setup --check failed with stderr:\n{}", stderr);
    assert!(
        stdout.contains("[Version]") || stdout.contains("0."),
        "setup --check should display version info, got:\n{}",
        stdout
    );
    assert!(
        stdout.contains("DIR") || stdout.contains("dir"),
        "setup --check should display directory paths, got:\n{}",
        stdout
    );
}

#[test]
fn setup_dump_config_outputs_valid_kdl() {
    let (stdout, stderr, code) = run_zellij(&["setup", "--dump-config"]);
    assert_no_panic(&stderr, "setup --dump-config");
    assert_eq!(
        code, 0,
        "setup --dump-config failed with stderr:\n{}",
        stderr
    );
    assert!(
        stdout.contains("keybinds"),
        "dump-config should contain 'keybinds', got:\n{}",
        &stdout[..stdout.len().min(500)]
    );
    assert!(
        stdout.contains("bind"),
        "dump-config should contain 'bind' entries"
    );
}

#[test]
fn setup_dump_layout_default_outputs_layout() {
    let (stdout, stderr, code) = run_zellij(&["setup", "--dump-layout", "default"]);
    assert_no_panic(&stderr, "setup --dump-layout default");
    assert_eq!(
        code, 0,
        "setup --dump-layout default failed with stderr:\n{}",
        stderr
    );
    assert!(
        stdout.contains("layout"),
        "dump-layout should contain 'layout', got:\n{}",
        stdout
    );
    assert!(
        stdout.contains("pane"),
        "dump-layout should contain 'pane' definitions"
    );
}

// ─── Session management (exercises IPC path construction) ─────────────────

#[test]
fn list_sessions_with_no_sessions_exits_cleanly() {
    // KEY REGRESSION TEST: exercises the code path that previously panicked on
    // Windows with "not a named pipe path".
    let (stdout, stderr, code) = run_zellij(&["list-sessions"]);
    assert_no_panic(&stderr, "list-sessions");
    assert!(
        code == 0 || code == 1,
        "list-sessions should exit 0 or 1, got {} with stderr:\n{}",
        code,
        stderr
    );
    // Output may contain "No active zellij sessions found" or a list of sessions
    // (possibly with ANSI escape codes). Either is fine — the important thing is
    // no panic occurred.
    let combined = format!("{}{}", stdout, stderr);
    assert!(
        !combined.is_empty(),
        "list-sessions should produce some output"
    );
}

#[test]
fn kill_all_sessions_with_no_sessions_exits_cleanly() {
    let (stdout, stderr, _code) = run_zellij(&["kill-all-sessions", "--yes"]);
    assert_no_panic(&stderr, "kill-all-sessions --yes");
    let combined = format!("{}{}", stdout, stderr);
    assert!(
        !combined.contains("panicked"),
        "kill-all-sessions panicked:\n{}",
        combined
    );
}

#[test]
fn attach_empty_session_name_rejected() {
    let (_stdout, stderr, code) = run_zellij(&["attach", ""]);
    assert_no_panic(&stderr, "attach empty name");
    assert_ne!(code, 0, "attaching with empty name should fail");
}

// ─── Session lifecycle test ───────────────────────────────────────────────
//
// Start a zellij server as a background process using the hidden --server flag,
// then verify that the named pipe is connectable via IPC, proving the full
// path_to_ipc_name -> ListenerOptions::create_sync -> Stream::connect round-trip
// works. Finally kill the server.

#[test]
fn session_lifecycle_server_ipc_round_trip() {
    use interprocess::local_socket::{prelude::*, Stream as LocalSocketStream};
    use std::time::Duration;

    let session_name = format!("inttest-{}", std::process::id());

    // Build the socket path the same way zellij does
    let sock_dir = zellij_sock_dir();
    std::fs::create_dir_all(&sock_dir).expect("failed to create sock dir");
    let socket_path = sock_dir.join(&session_name);

    // Start the server as a background child process
    let mut server_proc = Command::new(zellij_bin())
        .arg("--server")
        .arg(&socket_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start zellij server");

    // Wait for the server to create the listener
    std::thread::sleep(Duration::from_secs(3));

    // Verify we can connect to the server's IPC socket
    let ipc_name = zellij_utils::ipc::path_to_ipc_name(&socket_path)
        .expect("path_to_ipc_name should succeed");

    let connect_result = LocalSocketStream::connect(ipc_name);
    let connected = connect_result.is_ok();

    if let Ok(stream) = connect_result {
        // Send a ConnStatus query to verify the server is alive
        use zellij_utils::ipc::{IpcSenderWithContext, ClientToServerMsg};
        let mut sender: IpcSenderWithContext<ClientToServerMsg> =
            IpcSenderWithContext::new(stream);
        let send_result = sender.send_client_msg(ClientToServerMsg::ConnStatus);
        eprintln!("IPC ConnStatus send result: {:?}", send_result);
    }

    // Clean up: kill the server
    let _ = server_proc.kill();
    let status = server_proc.wait();
    eprintln!("Server exit status: {:?}", status);

    // Read server stderr for diagnostic info
    if let Some(mut stderr_pipe) = server_proc.stderr.take() {
        use std::io::Read;
        let mut stderr_output = String::new();
        let _ = stderr_pipe.read_to_string(&mut stderr_output);
        if !stderr_output.is_empty() {
            eprintln!("Server stderr: {}", &stderr_output[..stderr_output.len().min(2000)]);
        }
        assert_no_panic(&stderr_output, "server process");
    }

    // On Unix, clean up the socket file
    #[cfg(unix)]
    let _ = std::fs::remove_file(&socket_path);

    assert!(
        connected,
        "should be able to connect to the zellij server's IPC socket at {:?}",
        socket_path
    );
}

/// Reproduce ZELLIJ_SOCK_DIR for test purposes.
fn zellij_sock_dir() -> std::path::PathBuf {
    #[cfg(unix)]
    {
        let uid = unsafe { libc::getuid() };
        let base = std::env::var("XDG_RUNTIME_DIR")
            .or_else(|_| std::env::var("TMPDIR"))
            .unwrap_or_else(|_| "/tmp".to_string());
        std::path::PathBuf::from(base)
            .join(format!("zellij-{}", uid))
            .join("contract_version_1")
    }
    #[cfg(windows)]
    {
        // On Windows, ZELLIJ_SOCK_DIR = ZELLIJ_CACHE_DIR / contract_version_1
        // ZELLIJ_CACHE_DIR = ProjectDirs::from("org", "Zellij Contributors", "Zellij").cache_dir()
        // We replicate this using the directories crate (used by zellij-utils as `directories`)
        let cache_dir = std::env::var("LOCALAPPDATA")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir())
            .join("Zellij Contributors")
            .join("Zellij")
            .join("cache");
        cache_dir.join("contract_version_1")
    }
}
