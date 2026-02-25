# Claude Code Project Notes — zellij-windows

## MSYS2 / Git Bash Shell Quirks

### taskkill syntax
MSYS2 converts single-slash flags (`/F`) into file paths. Always use **double slashes**:
```bash
taskkill //F //IM zellij.exe
```
Never use `taskkill /F /IM` — MSYS2 mangles `/F` into `F:/`.

### Before rebuilding zellij.exe
Always kill all zellij processes first, or the linker fails with access denied:
```bash
taskkill //F //IM zellij.exe 2>/dev/null; sleep 2
```

## Build Commands

```bash
# Release build (Windows)
# Requires cmake in PATH (included with VS Build Tools)
taskkill //F //IM zellij.exe 2>/dev/null; sleep 2
export PATH="$PATH:/c/Program Files (x86)/Microsoft Visual Studio/2022/BuildTools/Common7/IDE/CommonExtensions/Microsoft/CMake/CMake/bin"
cargo build --release --no-default-features --features "plugins_from_target,web_server_capability"
```

## Windows ConPTY Notes

- ConPTY panes spawn `cmd.exe` by default
- `GenerateConsoleCtrlEvent(CTRL_C_EVENT)` is broken in ConPTY on Windows 11 Build 26200
- Ctrl+C uses a helper process (`--conpty-ctrl-c`) spawned inside the ConPTY to inject KEY_DOWN Ctrl+C via `WriteConsoleInput`
- See `ctrlc_investigation.md` for full technical details
