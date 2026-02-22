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

- `zellij-utils`: 205 pass, 21 fail (pre-existing -- Unix path snapshots vs Windows paths)
- `zellij-client`: 28 pass, 0 fail
- `zellij-server`: 1 pass (`get_cwd`), 0 fail, 3 skipped (Unix signal tests, correctly `cfg(not(windows))`)

## Tests to Add

### 1. Server PTY Backend (`os_input_output_windows.rs`) -- HIGH PRIORITY

Now unblocked. Add to `zellij-server/src/unit/os_input_output_tests.rs`:

- `spawn_terminal_runs_command` -- Spawn `cmd.exe /C echo hello`, read output from
  AsyncReader, verify "hello" appears
- `spawn_terminal_returns_valid_pid` -- Spawn a process, verify pid > 0
- `spawn_terminal_nonexistent_command_errors` -- Spawn nonexistent binary, expect
  CommandNotFound error
- `spawn_terminal_with_failover` -- Bad primary + valid failover, verify failover runs
- `set_terminal_size` -- Spawn PTY, resize, verify no error
- `write_to_tty_stdin` -- Spawn cmd.exe, write bytes, verify Ok
- `tcdrain_flushes_without_error` -- Spawn PTY, call tcdrain, verify Ok
- `kill_terminates_process` -- Spawn long-running process, kill, verify quit_cb fires
- `force_kill_terminates_process` -- Same (both use TerminateProcess on Windows)
- `next_terminal_id_increments` -- Call next_terminal_id + reserve multiple times,
  verify monotonic increase
- `clear_terminal_id_removes_entry` -- Reserve then clear, verify ID is available again
- `async_reader_delivers_data` -- Spawn process that prints, verify WindowsAsyncReader
  delivers it
- `async_reader_returns_eof_on_exit` -- Spawn short-lived process, verify Ok(0) eventually

### 2. Client Signal Handling (`os_input_output_windows.rs`) -- MEDIUM PRIORITY

Add to `zellij-client/src/os_input_output_windows.rs` or a new test module:

- `async_signal_listener_can_be_constructed` -- Verify AsyncSignalListener::new() returns Ok
- `blocking_signal_iterator_can_be_constructed` -- Verify BlockingSignalIterator::new()
  returns Ok
- `resize_detection_fires_on_size_change` -- Integration-level; needs actual console resize
  or mock
- `blocking_iterator_quit_on_ctrl_c` -- Integration-level; needs real console control event

### 3. `get_default_shell()` (`pty.rs`) -- MEDIUM PRIORITY

Now unblocked. Add to a test module in pty.rs:

- `default_shell_returns_comspec_on_windows` -- Set COMSPEC, verify path returned
- `default_shell_falls_back_to_cmd_exe` -- Unset COMSPEC, verify "cmd.exe" returned

Note: env var tests need `#[serial]` since env vars are process-global.

### 4. Windows Constants (`consts.rs`) -- LOW PRIORITY

Add to `zellij-utils/src/consts.rs`:

- `windows_tmp_dir_is_under_temp` -- Verify ZELLIJ_TMP_DIR starts with system temp_dir()
- `windows_sock_dir_is_valid_path` -- Verify ZELLIJ_SOCK_DIR is a valid path
- `windows_log_paths_are_children_of_tmp` -- Verify ZELLIJ_TMP_LOG_DIR is under
  ZELLIJ_TMP_DIR

### 5. Cross-platform Hooks (`os_input_output.rs`) -- LOW PRIORITY

Now unblocked.

- `run_command_hook_on_windows` -- Run `echo %RESURRECT_COMMAND%` hook, verify output

### 6. Server Launch (`commands.rs`) -- LOW PRIORITY

Hard to unit test (involves process re-exec). Best verified via integration/E2E testing.

- `server_mode_env_var_prevents_re_exec` -- Set ZELLIJ_SERVER_MODE=1, verify no re-spawn

### 7. Windows Signal Delivery Tests (parallels to Unix tests) -- MEDIUM PRIORITY

Add Windows versions of the existing `#[cfg(not(windows))]` tests in
`os_input_output_tests.rs`:

- `windows_kill_terminates_process` -- Spawn `timeout /T 60`, kill via backend, verify exits
- `windows_force_kill_terminates_process` -- Same with force_kill
- `windows_send_sigint_to_process` -- Spawn `cmd.exe`, send_sigint, verify no panic
