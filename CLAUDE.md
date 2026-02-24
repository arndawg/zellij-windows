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
# Release build (Windows, no default features)
taskkill //F //IM zellij.exe 2>/dev/null; sleep 2
cargo build --release --no-default-features --features plugins_from_target
```

## Windows ConPTY Notes

- ConPTY panes spawn `cmd.exe` by default
- `GenerateConsoleCtrlEvent(CTRL_C_EVENT)` is broken in ConPTY on Windows 11 Build 26200
- Ctrl+C uses a helper process (`--conpty-ctrl-c`) spawned inside the ConPTY to inject KEY_DOWN Ctrl+C via `WriteConsoleInput`
- See `ctrlc_investigation.md` for full technical details
