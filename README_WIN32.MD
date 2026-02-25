# zellij Windows Port

This is an experimental Windows port of [zellij](https://github.com/zellij-org/zellij),
a terminal workspace with multiplexer capabilities. It builds and runs on
Windows 10+ using the ConPTY API.

## Status: Experimental

This port is functional for basic terminal multiplexing but has known limitations.
It is based on the upstream zellij codebase with ~43 Windows-specific commits adding
ConPTY support, Windows IPC, signal handling, and platform abstractions.

## What Works

- **Pane management**: Split panes horizontally and vertically
- **Tab management**: Create, switch, and close tabs
- **ConPTY backend**: Each pane runs in its own Windows Pseudo Console
- **Keyboard input**: Standard input including Ctrl+C signal delivery
- **Terminal rendering**: Full VT output via crossterm with Windows support
- **Plugin loading**: WASI plugins load from the target directory
- **Session management**: Named sessions with IPC via Windows named pipes
- **Resize**: Dynamic terminal and pane resize

## What Doesn't Work / Known Limitations

- **No web server**: Built with `--no-default-features` — the web client
  capability (`web_server_capability`) is disabled
- **No vendored curl**: The `vendored_curl` feature is disabled (avoids
  OpenSSL build complexity on Windows)
- **ConPTY output lag with mode 2026**: Applications that use synchronized
  output (ESC[?2026h/l), such as Claude Code's Ink framework, cause ConPTY's
  conhost to buffer output for 1-4 seconds per render cycle. This is a Windows
  platform limitation — tmux has identical stalls. See `ctrlc_investigation.md`
  for the full 16-session investigation
- **Ctrl+C delivery**: Uses a PeekConsoleInput helper process spawned inside
  the ConPTY to detect whether the child consumes stdin. Works for interactive
  programs (Claude Code, node) and non-interactive programs (ping, dir /s)
  but has edge cases during application startup
- **Sixel/image support**: Disabled (DA1 reports VT100+AVO to match tmux)

## Installation

### From GitHub Releases

1. Download the latest `zellij-windows-*.zip` from
   [GitHub Releases](https://github.com/arndawg/zellij-windows/releases)
2. Extract `zellij.exe` to a folder in your `%PATH%`
3. Run `zellij`

### Build from Source

Requires Rust toolchain and Protobuf compiler:

```powershell
# Install protoc (via chocolatey, scoop, or manually)
choco install protoc

# Build
cargo build --release --no-default-features --features plugins_from_target

# Binary is at target/release/zellij.exe
```

The build produces a single standalone `zellij.exe` (38MB) with no DLL
dependencies beyond the Windows system libraries.

## ConPTY Configuration

The port uses the following ConPTY settings, tuned through extensive testing
against tmux's implementation:

| Setting | Value | Reason |
|---------|-------|--------|
| `CreatePseudoConsole` flags | `0` | `INHERIT_CURSOR` causes DSR stall; `RESIZE_QUIRK` causes repaint stalls |
| Output pipe buffer | ~4KB (default) | Matches tmux; forces eager flushing |
| DA1 response | `ESC[?1;2c` (VT100+AVO) | Matches tmux; avoids heavy sixel rendering paths |

## Architecture

The Windows port adds platform-specific implementations while preserving the
upstream architecture:

- **PTY backend**: `os_input_output_windows.rs` — ConPTY creation, async
  reader with TCP socketpair bridge (matching tmux's bridge thread pattern),
  Ctrl+C signal delivery via helper process
- **ConPTY patches**: `portable-pty-patch/` — forked portable-pty with
  Windows-specific fixes (flag tuning, pipe buffer, sideload support)
- **IPC**: Windows named pipes for session discovery, replacing Unix domain
  sockets
- **Plugins**: WASI plugins load from the build target directory with
  Windows path normalization

## Runtime Requirements

- Windows 10 version 1809+ (for ConPTY support)
- Windows Terminal or a VT-capable console host

## Debugging

Zellij logs to `%LOCALAPPDATA%\Temp\zellij\zellij-log\zellij.log`. Increase
verbosity with:

```powershell
zellij --debug
```

## Related

- [zellij](https://github.com/zellij-org/zellij) — upstream project
- [tmux-windows](https://github.com/arndawg/tmux-windows) — tmux Windows port by the same author
- [ctrlc_investigation.md](ctrlc_investigation.md) — detailed ConPTY investigation notes (16 sessions)
