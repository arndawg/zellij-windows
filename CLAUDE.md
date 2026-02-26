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

### Rebuilding WASM plugins (required after changing plugin source)

In release mode, plugins are ALWAYS loaded from the static assets in
`zellij-utils/assets/plugins/*.wasm` — the `plugins_from_target` feature only affects
debug builds. After changing any plugin source (e.g. `default-plugins/session-manager/`),
rebuild and copy before the main build:

```bash
# Rebuild a specific plugin (e.g. session-manager)
cd default-plugins/session-manager
cargo build --release --target wasm32-wasip1
cp ../../target/wasm32-wasip1/release/session-manager.wasm \
   ../../zellij-utils/assets/plugins/session-manager.wasm
cd ../..
# Then do the normal release build
```

## Winget Package Submission

Package: `arndawg.zellij-windows` in `microsoft/winget-pkgs`.

### How to submit a new version

1. **Sync the fork** before anything else:
   ```bash
   gh api repos/arndawg/winget-pkgs/merge-upstream -X POST -f branch=master
   ```

2. **Create a branch** from the synced master:
   ```bash
   MASTER_SHA=$(gh api repos/arndawg/winget-pkgs/git/refs/heads/master --jq '.object.sha')
   gh api repos/arndawg/winget-pkgs/git/refs -X POST \
     -f ref=refs/heads/arndawg.zellij-windows-VERSION \
     -f sha="$MASTER_SHA"
   ```

3. **Create files via the contents API** (one PUT per file). Do NOT use the git trees API — on a repo this large it produces phantom diffs with hundreds of unrelated files. The contents API creates clean per-file commits:
   ```bash
   CONTENT=$(printf '%s' "$YAML" | base64 -w0)
   gh api repos/arndawg/winget-pkgs/contents/manifests/a/arndawg/zellij-windows/VERSION/FILENAME \
     -X PUT -f message="Add manifest" -f content="$CONTENT" \
     -f branch=arndawg.zellij-windows-VERSION
   ```
   Three files needed: `arndawg.zellij-windows.installer.yaml`, `arndawg.zellij-windows.locale.en-US.yaml`, `arndawg.zellij-windows.yaml`.

4. **Verify** the branch diff before creating the PR:
   ```bash
   gh api repos/arndawg/winget-pkgs/compare/master...arndawg.zellij-windows-VERSION \
     --jq '{ahead_by: .ahead_by, files: [.files[].filename]}'
   ```
   Must show exactly 3 files and 3 commits ahead.

5. **Create the PR** with `gh pr create --repo microsoft/winget-pkgs`.

6. **Validation takes up to 3 hours.** Monitor with:
   ```bash
   gh pr view PRNUM --repo microsoft/winget-pkgs --json labels,comments \
     --jq '{labels: [.labels[].name], latest: .comments[-1].body[:200]}'
   ```
   Look for the `Validation-Completed` label.

### Manifest formatting

Do **not** add a trailing blank line to manifest YAML files.

## Windows ConPTY Notes

- ConPTY panes spawn `cmd.exe` by default
- `GenerateConsoleCtrlEvent(CTRL_C_EVENT)` is broken in ConPTY on Windows 11 Build 26200
- Ctrl+C uses a helper process (`--conpty-ctrl-c`) spawned inside the ConPTY to inject KEY_DOWN Ctrl+C via `WriteConsoleInput`
- See `ctrlc_investigation.md` for full technical details
