# Windows Port Testing TODO

## Test Blocker: RESOLVED

The `sysinfo 0.22.5` -> `ntapi 0.3.7` compile failure has been fixed by upgrading
sysinfo to 0.34 and migrating the API:

- Removed `ProcessExt`, `SystemExt` trait imports (traits removed in sysinfo 0.31+)
- `Pid::from(pid as i32)` -> `Pid::from_u32(pid)`
- `refresh_process_specifics(pid, ProcessRefreshKind::default())` ->
  `refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, ProcessRefreshKind::nothing().with_cwd(...))`
- `process.cwd()` returns `Option<&Path>` instead of `&Path`
- `process.cmd()` returns `&[OsString]` instead of `&[String]`

All three crates (`zellij-utils`, `zellij-client`, `zellij-server`) now compile and
their existing tests pass on Windows.

## Current Test Results

- `zellij-utils`: 209 pass (205 pre-existing + 4 new Windows constants), 21 fail (pre-existing -- Unix path snapshots vs Windows paths)
- `zellij-client`: 30 pass (28 pre-existing + 2 new signal construction smoke tests), 0 fail
- `zellij-server`: 15 pass (1 pre-existing `get_cwd` + 4 terminal ID management + 10 Windows PTY tests), 0 fail, 3 skipped (Unix signal tests, correctly `cfg(not(windows))`)

## Implemented Tests

### 1. Server PTY Backend -- DONE (10 tests)

In `zellij-server/src/unit/os_input_output_tests.rs`:

- `spawn_terminal_returns_valid_pid` -- Spawn `cmd.exe /C echo hello`, verify pid > 0
- `spawn_terminal_nonexistent_command_errors` -- Spawn nonexistent binary, expect error
- `spawn_terminal_with_failover` -- Bad primary + valid failover, verify failover runs
- `spawn_terminal_runs_command_and_produces_output` -- Spawn `cmd.exe /C echo hello_from_pty`,
  read output via AsyncReader (with DSR response to unblock ConPTY), verify output contains expected text
- `async_reader_returns_eof_on_child_exit` -- Spawn short-lived process, respond to DSR,
  wait for quit callback, drop master handle, verify EOF/error from reader
- `set_terminal_size_succeeds` -- Spawn PTY, resize to 120x40, verify no error
- `write_to_tty_stdin_succeeds` -- Spawn cmd.exe, write bytes, verify Ok
- `tcdrain_succeeds` -- Spawn PTY, call tcdrain, verify Ok
- `kill_terminates_spawned_process` -- Spawn long-running process, kill, verify quit_cb fires
- `force_kill_terminates_spawned_process` -- Same with force_kill

Cross-platform terminal ID management tests (also in `os_input_output_tests.rs`):

- `next_terminal_id_starts_at_zero` -- Verify first ID is 0
- `next_terminal_id_increments_after_reserve` -- Reserve IDs, verify monotonic increase
- `clear_terminal_id_removes_entry` -- Reserve then clear, verify ID reuse
- `reserve_and_clear_all_returns_to_zero` -- Reserve+clear all, verify back to 0

### 2. Client Signal Handling -- DONE (2 smoke tests)

In `zellij-client/src/os_input_output_windows.rs`:

- `async_signal_listener_can_be_constructed` -- Verify AsyncSignalListener::new() returns Ok
- `blocking_signal_iterator_can_be_constructed` -- Verify BlockingSignalIterator::new() returns Ok

### 3. Windows Constants -- DONE (4 tests)

In `zellij-utils/src/consts.rs`:

- `windows_tmp_dir_is_under_system_temp` -- Verify ZELLIJ_TMP_DIR starts with temp_dir()
- `windows_log_dir_is_under_tmp_dir` -- Verify ZELLIJ_TMP_LOG_DIR under ZELLIJ_TMP_DIR
- `windows_log_file_is_under_log_dir` -- Verify ZELLIJ_TMP_LOG_FILE under ZELLIJ_TMP_LOG_DIR
- `windows_sock_dir_is_nonempty_path` -- Verify ZELLIJ_SOCK_DIR is non-empty

## ConPTY Testing Notes

ConPTY sends a Device Status Report (`ESC[6n`) during initialization and blocks the
entire terminal session until the "terminal" responds with a cursor position report
(`ESC[row;colR`). Tests that read ConPTY output must:

1. Read from the async reader to receive the DSR query
2. Respond by writing `\x1b[1;1R` to the PTY stdin
3. Only then will ConPTY deliver the actual command output

ConPTY also keeps the read pipe open even after the child process exits. To get EOF
from the reader, you must drop the master handle (e.g., via `clear_terminal_id()`),
which closes the ConPTY and causes the reader to get an error/EOF.

## Remaining Tests (lower priority)

### `get_default_shell()` (`pty.rs`) -- MEDIUM PRIORITY

- `default_shell_returns_comspec_on_windows` -- Set COMSPEC, verify path returned
- `default_shell_falls_back_to_cmd_exe` -- Unset COMSPEC, verify "cmd.exe" returned

Note: env var tests need `#[serial]` since env vars are process-global.

### Cross-platform Hooks (`os_input_output.rs`) -- LOW PRIORITY

- `run_command_hook_on_windows` -- Run `echo %RESURRECT_COMMAND%` hook, verify output

### Server Launch (`commands.rs`) -- LOW PRIORITY

Hard to unit test (involves process re-exec). Best verified via integration/E2E testing.

### Integration-level Signal Tests -- LOW PRIORITY

- `resize_detection_fires_on_size_change` -- Needs actual console resize or mock
- `blocking_iterator_quit_on_ctrl_c` -- Needs real console control event
