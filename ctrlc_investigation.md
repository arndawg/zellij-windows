# Ctrl+C Investigation for Windows ConPTY

## The fundamental ConPTY bug

`GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0)` is broken in ConPTY on Windows 11
Build 26200. It returns success (1) but never delivers the event. Writing 0x03
to the ConPTY pipe generates a KEY_UP event (not KEY_DOWN), so
`ENABLE_PROCESSED_INPUT` never triggers `CTRL_C_EVENT`.

Both approaches were tested from a helper process spawned INSIDE the ConPTY
(native console member):
- `GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0)` → returns 1, no effect
- `WriteConsoleInputW` with KEY_DOWN Ctrl+C → returns 1, no effect

This means there is NO WAY to generate a real `CTRL_C_EVENT` in ConPTY on
this Windows 11 build. All workarounds must use other mechanisms.

## The conflict: Claude Code vs ping

| Mechanism | Claude Code | ping -t | dir /s |
|-----------|-------------|---------|--------|
| 0x03 byte | PASS (reads stdin) | FAIL (doesn't read stdin) | FAIL (built-in) |
| Ctrl+Break VT | KILLED (no SIGBREAK handler) | shows stats, continues | PASS |
| TerminateProcess | KILLED immediately | PASS | N/A (built-in) |
| Delayed terminate (100ms) | shows "Interrupted" then killed | PASS | PASS |

No single mechanism works for both Claude Code and ping.

## Previous approach: 0x03 + delayed TerminateProcess (REJECTED)

```
write_to_tty_stdin when buf == [0x03]:
  1. Write 0x03 to pipe (always)
  2. If no descendants: Ctrl+Break VT (for built-in commands like dir /s)
  3. If has descendants: spawn thread → sleep 100ms → terminate_descendants
```

Results: ping PASS, dir PASS, Claude Code FAIL (killed after 100ms).

## Current approach: PeekConsoleInput detection helper

Key insight: programs that read stdin (Claude Code) CONSUME the 0x03 KEY event
from the console input buffer. Programs that don't read stdin (ping) leave it
unconsumed. We can detect this by peeking the buffer after a delay.

```
write_to_tty_stdin when buf == [0x03]:
  1. Write 0x03 to pipe (always — for stdin readers like Claude Code)
  2. If no descendants: Ctrl+Break VT (for built-in commands like dir /s)
  3. If has descendants: spawn `zellij.exe --conpty-ctrl-c` INSIDE the ConPTY
     - Helper sleeps 100ms (give time for stdin readers to consume)
     - Helper opens CONIN$ and PeekConsoleInputW
     - If 0x03 KEY_EVENT still in buffer → exit code 42 → server terminates descendants
     - If 0x03 consumed → exit code 0 → server does nothing
```

### Implementation files
- `src/main.rs`: `--conpty-ctrl-c` early exit with PeekConsoleInput logic
- `portable-pty-patch/src/lib.rs`: `spawn_command_in_pty` trait method on MasterPty
- `portable-pty-patch/src/win/conpty.rs`: implementation using PsuedoCon::spawn_command
- `Cargo.toml`: windows-sys dependency for Win32_System_Console etc.
- `zellij-server/src/os_input_output_windows.rs`: spawn_ctrl_c_helper + write_to_tty_stdin

### Test results
- ping -t: PASS (user confirmed)
- dir /s c:\: PASS (user confirmed)
- Claude Code: PASS (user confirmed)

## Input path trace
```
User Ctrl+C → stdin (0x03) → client parser → ClientToServerMsg::Key(raw_bytes=[0x03])
→ route.rs → Action::Write → ScreenInstruction::WriteCharacter
→ tab.write_to_active_terminal → pane.adjust_input_to_terminal
→ PtyWriteInstruction::Write([0x03]) → pty_writer.rs
→ os_input.write_to_tty_stdin(terminal_id, &[0x03])
```

`send_sigint` is NOT called for user Ctrl+C — only by plugins programmatically.

## Technical details

### How tmux-windows handles it
tmux just writes raw byte 0x03 to the ConPTY pipe. No process termination.
Programs that read stdin (like node.js/Claude Code) see the 0x03 byte and
handle it as Ctrl+C gracefully. Ping doesn't stop in tmux either.
File: `c:\src\tmux\win32\win32-pty.c`

### Why 0x03 doesn't stop ping in ConPTY
`ping.exe` doesn't read stdin — it relies on `CTRL_C_EVENT` from the console
subsystem. In ConPTY, writing 0x03 to the pipe generates KEY_UP (not KEY_DOWN),
so `ENABLE_PROCESSED_INPUT` never fires `CTRL_C_EVENT`. The 0x03 sits in the
console input buffer unconsumed.

### Why Ctrl+Break kills Claude Code
Node.js/libuv maps `CTRL_BREAK_EVENT` → `SIGBREAK`. Claude Code does NOT
register `process.on('SIGBREAK')`. Without a handler, libuv returns FALSE
and the default handler calls `ExitProcess`.

### ping.exe behavior
- **CTRL+C** → prints statistics and **quits**
- **CTRL+BREAK** → prints statistics and **continues**

So Ctrl+Break doesn't even fully stop `ping -t`.

## Possible improvements

**A. Longer delay with descendant exit check**
Instead of always terminating after 100ms, check if descendant count DECREASED.
If some exited (handled Ctrl+C themselves), don't terminate remaining ones.
Problem: Claude Code doesn't exit, so count stays the same.

**B. Process name allowlist**
Skip termination for known interactive programs (node.exe, python.exe, etc.).
Fragile but covers the main use case.

**C. Upstream: Claude Code adds SIGBREAK handler**
If Claude Code registered `process.on('SIGBREAK', handler)`, we could send
Ctrl+Break unconditionally. Both Claude Code and ping would handle it.

**D. Two-phase Ctrl+C (user explicitly rejected)**
First press: 0x03 only. Second press within 2s: terminate descendants.
User said "remember not to add an escalating double press ctrl-c."

## Status
- [x] Confirmed GenerateConsoleCtrlEvent broken in ConPTY (helper process test)
- [x] Confirmed WriteConsoleInputW doesn't trigger CTRL_C_EVENT
- [x] Implemented delayed terminate (100ms) approach — REJECTED (kills Claude Code)
- [x] Implemented PeekConsoleInput detection helper — ALL PASS, committed
- [x] Investigate input/output lag when launching Claude Code inside zellij

---

# Input/Output Lag Investigation

## Observed behavior

When running Claude Code inside zellij, there are **1-4 second freezes** where
typed characters don't appear on screen. Notably, tmux (`c:\src\tmux`) does NOT
have this issue with the same system conhost.exe.

## Key discovery: NOT a conhost SRWLOCK issue

**Original hypothesis (DISPROVEN):** conhost.exe's unfair SRWLOCK causes output
stalls. We sideloaded OpenConsole.exe (Windows Terminal v1.23 with PR #17510 fix)
to bypass system conhost.

**Result:** Sideloading is confirmed active (OpenConsole.exe spawns, log confirms
`Using sideloaded conpty.dll`). Bulk throughput improved dramatically (14K+ fast
reads, 1.15 MB during `dir /s`). But **1-4 second stalls persist** during Claude
Code operation, with the same pattern as before.

**Meanwhile, tmux uses system conhost.exe (no sideloading) and is responsive.**
This proves the stalls are NOT caused by conhost's SRWLOCK.

## OpenConsole.exe startup queries (hex dump)

Captured the first 20 reads from ConPTY output pipe after sideloaded
OpenConsole.exe starts:

```
Read #1:  ESC[1t                          — XTWINOPS: de-iconify
Read #2:  ESC[6n ESC[c ESC[?1004h ESC[?9001h — DSR, DA, focus reporting, win32-input-mode
Read #3:  ESC[1;1H                        — cursor home
Read #4:  ESC[?7l                         — disable autowrap
Read #5:  ESC[?7h                         — enable autowrap
Read #6:  "Microsoft Windows [Version..." — cmd.exe banner (43 bytes)
Read #7:  \r\n                            — newline
Read #8:  "(c) Microsoft Corporation..." — copyright (47 bytes)
Read #9-11: \r\n, \r\n, "C:\Users\arnib>" — prompt
```

All 11 reads complete in **13ms total**. The pre-sent DSR/DA responses arrive
at the right time (line 455-456 in log). Initial pane startup is FAST.

**Note:** ESC[?9001h means OpenConsole.exe wants win32-input-mode enabled, even
though we create the console WITHOUT the PSEUDOCONSOLE_WIN32_INPUT_MODE flag.

## What's actually different: zellij vs tmux

### CreatePseudoConsole flags

| | tmux | zellij |
|--|------|--------|
| **Flags** | `0` (none) | `INHERIT_CURSOR \| RESIZE_QUIRK` (0x3) |
| **File** | `win32/win32-pty.c:103` | `portable-pty-patch/src/win/psuedocon.rs:87-88` |

`INHERIT_CURSOR` causes ConPTY to send ESC[6n to discover cursor position.
`RESIZE_QUIRK` causes ConPTY to repaint the entire screen on resize.
tmux uses neither — it passes `0` and avoids both behaviors.

### Pipe buffer sizes

| | tmux | zellij |
|--|------|--------|
| **stdin pipe** | default (~4KB) | default (~4KB) |
| **stdout pipe** | default (~4KB) | 1MB |

### Architecture (critical difference)

**tmux:** simple bridge threads, ~zero overhead per byte
```
ConPTY output pipe → ReadFile(4KB) → send() to socket → libevent callback → render
ConPTY input pipe ← WriteFile() ← recv() from socket ← libevent write
```
Two dedicated threads per pane. Blocking I/O. No channels, no async framework.
Single-process event loop with direct function calls.

**zellij:** complex async pipeline, multiple hops per byte
```
ConPTY output pipe → ReadFile(8KB) → mpsc::channel(64) → tokio async recv
→ spawn_blocking(send_to_screen) → Screen thread bus.recv() → VT parse
→ BackgroundJob::RenderToClients → debounce 10ms → render_to_clients
→ client channel → crossterm output
```

### Sideloaded ConPTY

| | tmux | zellij |
|--|------|--------|
| **conpty.dll** | no (system conhost.exe) | yes (sideloaded OpenConsole.exe) |
| **OpenConsole.exe** | no | yes, v1.23.251008001 |

tmux is responsive with system conhost. The sideloaded ConPTY didn't fix
zellij's stalls. This confirms the bottleneck is in **how zellij interacts
with ConPTY**, not in conhost itself.

## Diagnostic logging

Log file: `%TEMP%\zellij\zellij-log\zellij.log`

Logging points (only fires when threshold exceeded):
- `[pty_reader] read #N: Xms, N bytes, hex=[...]` — first 20 reads with full hex dump
- `[pty_reader] read #N blocked for Xms` — ConPTY pipe read stall (>500ms)
- `[pty_reader] channel send blocked for Xms` — backpressure from screen (>100ms)
- `[async_reader] channel recv waited Xms` — async reader stall (>500ms)
- `[terminal_bytes] tid=N async_reader.read waited Xms` — terminal_bytes stall (>500ms)
- `[terminal_bytes] tid=N send_to_screen blocked for Xms` — screen backpressure (>100ms)
- `[screen] handle_pty_bytes pid=N took Xms` — VT parsing time (>100ms)
- `[screen] render_to_clients took Xms` — rendering time (>100ms)
- `[screen] event processing took Xms` — total event time (>200ms)
- `[pty_writer] write+drain tid=N took Xms` — input write stall (>100ms)
- `[pty_writer] INPUT tid=N ...` — every keystroke with hex + ASCII

**Key log findings:**
- No channel send backpressure ever triggered
- No handle_pty_bytes >100ms ever triggered
- No render_to_clients >100ms ever triggered
- No screen event >200ms ever triggered
- No pty_writer write stall >100ms ever triggered
- ReadFile stalls of 1-4 seconds persist, returning tiny amounts (41-74 bytes)

This means zellij's pipeline processes data fast once it arrives. The bottleneck
is in ConPTY itself not producing output to the pipe for seconds at a time.

## Stall pattern during Claude Code operation

```
13:03:43.186  pty_writer: "a" (user types)
13:03:43.256  pty_writer: "s"
13:03:43.277  pty_writer: "d"
... (30+ keystrokes over 2.5 seconds, all written immediately to ConPTY)
13:03:45.720  pty_reader: read #13064 blocked 2440ms, 74 bytes ← ConPTY finally produces output
```

Keystrokes reach ConPTY instantly (pty_writer confirms). But ConPTY holds the
output pipe for 2.4 seconds, returning only 74 bytes. This is NOT a zellij
pipeline issue — it's ConPTY not producing output despite receiving input.

## Experiments tried

| Experiment | Result |
|-----------|--------|
| channel(64→512) | no change |
| read buffer(8KB→64KB) | no change |
| REPAINT_DELAY(10→3ms) | no change |
| Sideloaded OpenConsole.exe v1.23 | bulk throughput improved, stalls persist |
| Pre-send DSR response (ESC[1;1R) | initial startup fast (13ms to prompt) |
| Pre-send DA response (ESC[?62;4c) | initial startup fast |
| flags=0 + sideloaded OpenConsole | cmd prompt echo fast (13ms), Claude Code stalls persist (2.4s) |
| flags=0 + system conhost | 5.8s startup stall (pre-sent DSR confusion), cmd echo 1-byte/read |
| **output pipe 4KB + sideloaded + flags=0 + no pre-sent DSR** | CMD echo fast (~1ms), Claude Code stalls persist (2.4s) |
| **DA1 response changed to VT100 (ESC[?1;2c)** | CMD echo fast, Claude Code stalls persist (2.3s) — identical to VT220 |
| **Socket bridge (TCP socketpair matching tmux)** | Stalls persist (0.5-2.7s) — identical to mpsc channel |
| **Fast VT response (Option G — respond in bridge thread)** | Stalls persist (1.5-3.2s) — response in <1ms but ConPTY still stalls |
| **Minimal ConPTY test (Option H — no zellij, just portable-pty)** | Stalls persist (2.4-6.3s) — eliminates zellij as cause |
| **System kernel32 ConPTY (no sideloaded conpty.dll)** | Stalls persist (2.4-6.3s) — same as sideloaded |
| **Strip VT queries + filter DA responses (Session 11)** | Stalls persist — filtering was moot (system conhost doesn't send DA1 query) |
| **minimal-conpty → tmux → Claude Code (Session 12)** | NO STALLS — tmux absorbs mode 2026, sets TERM_PROGRAM=tmux |
| **TERM_PROGRAM=tmux in child env (Session 13)** | Stalls persist (1.7-2.3s) — Claude Code uses mode 2026 unconditionally |

### Detailed session analysis

**Session 1 (12:23) — flags=0x3 + system conhost:**
- cmd prompt typing "claude" + "asdasd": 1.2-2.6s stalls during rapid typing
- ConPTY batches output into bursts during rapid input

**Session 2 (12:47) — flags=0x3 + sideloaded OpenConsole:**
- Bulk throughput: 14K reads (1.15MB) for `dir /s c:\` in 8 seconds
- Claude Code stalls persist: 2.3s, 1.0s, 1.4s, 4.0s between reads
- Stalls return small amounts (17-72 bytes)

**Session 3 (13:03) — flags=0 + sideloaded OpenConsole:**
- Initial startup: 13ms to cmd prompt (reads #1-11 all 0-2ms)
- CMD prompt echo: near-instant (reads #12-20 each ~25-186ms, matching typing speed)
- Each echo is 15 bytes: cursor-position + char + cursor-position
- Claude Code stalls persist: 2.4s, 2.6s between reads

**Session 4 (13:42) — flags=0 + system conhost + pre-sent DSR:**
- Read #1: `ESC[?9001h ESC[?1004h` (no ESC[6n — no DSR query with flags=0!)
- Reads #2-4: cmd banner + TWO full screen repaints (255 bytes each)
- Read #5: **5827ms stall**, 1 byte — caused by unsolicited `ESC[1;1R` confusing conhost
- Each echo is 1 byte only (no cursor positioning) — worse than sideloaded
- System conhost + flags=0 is **strictly worse** than sideloaded OpenConsole

**Session 5 (13:59) — 4KB pipe + sideloaded + flags=0 + no pre-sent DSR:**
- Sideloading confirmed, OpenConsole sends `ESC[c ESC[?1004h ESC[?9001h` (NO ESC[6n!)
- zellij's VT parser auto-responds to ESC[c with `ESC[?62;4c` (VT220+sixel)
- Initial startup: 22ms to prompt (reads #1-10, 0-11ms each)
- CMD prompt echo: near-instant (~1ms from keystroke to echo)
- `dir c:\ /s`: fast bulk output, Ctrl+C works
- `ping -t localhost`: echo responsive at cmd prompt
- After launching `claude`: rapid "asdasd" typing → **2.4s stall**, 72 bytes burst
- **4KB pipe had NO effect on Claude Code stalls — identical to 1MB pipe**

**Session 6 (14:42) — DA1 response changed to VT100+AVO (ESC[?1;2c):**
- Sideloading confirmed, flags=0, 4KB pipe, no pre-sent DSR
- DA1 response confirmed changed: pty_writer log shows `ESC[?1;2c` at 14:42:12.560
- CMD prompt echo: fast (41-155ms, matching typing speed)
- After launching `claude`: rapid "asdasd" typing → **2.3s stall** (read #37 blocked 2308ms, 70 bytes)
- Rapid typing from 15.884s to 19.895s+ with no echo output for 2.3 seconds
- **DA1 response had ZERO effect on Claude Code stalls — identical to VT220+sixel**
- **All ConPTY-level configuration changes have been exhausted with no improvement**

**Session 7 (15:09) — Socket bridge (TCP socketpair matching tmux architecture):**
- Replaced tokio::sync::mpsc::channel with TCP localhost socketpair
- Bridge thread: ReadFile(ConPTY pipe) → write_all(TCP socket), 4KB buffer (same as tmux)
- Async reader: tokio::net::TcpStream::read() (same as tmux's libevent bufferevent on socket)
- TCP_NODELAY enabled on both ends
- Sideloading confirmed, flags=0, 4KB pipe, VT100 DA1
- CMD prompt echo: 118-237ms (matching typing speed)
- Stalls during Claude Code:
  - read #39: 620ms, 44 bytes
  - read #52: **2382ms**, 61 bytes (rapid "sf" typing)
  - read #100: 1079ms, 201 bytes (rapid backspaces)
  - read #143: 1812ms, 158 bytes (Ctrl+C area)
  - read #170: **2721ms**, 17 bytes (cmd prompt after Claude exit)
  - read #197: **2511ms**, 60 bytes (rapid "sl" typing in second Claude run)
- **Socket bridge had ZERO effect — stalls identical to mpsc channel (0.5-2.7s vs 0.5-2.4s)**
- **This definitively eliminates I/O bridge architecture as the cause**
- All differences between tmux and zellij at the ConPTY level have now been tested and eliminated

**Session 8 (15:51) — Fast VT response path (Option G):**
- pty_reader bridge thread now has cloned ConPTY input pipe handle
- Scans output for VT queries (DA1, Secondary DA, DSR, DECXCPR) and responds IMMEDIATELY
- Sideloading confirmed, flags=0, 4KB pipe, VT100 DA1, TCP socketpair
- **DA1 intercepted and responded to in <1ms** — `[fast_vt] DA1 query detected, responded ESC[?1;2c`
- **Side effect: response echoed on screen** — read #11 shows `^[[?1;2c` printed literally at cursor position
  (cmd.exe echoes keyboard input, and our response was written to ConPTY input pipe)
- **Side effect: duplicate responses** — grid pipeline also sent DA1 response, cmd.exe tried to execute:
  `'.[?1' is not recognized as an internal or external command`
- CMD prompt echo was fast (reads #1-#11 all 0-6ms; longer gaps like read #12 at 3210ms
  were user typing pauses, NOT ConPTY stalls)
- Stalls during Claude Code startup/operation:
  - read #45: **1610ms**, 2264 bytes (Claude Code startup burst)
  - read #60: **2352ms**, 62 bytes (rapid "alalal" typing during Claude startup)
- User reports: stalls only during Claude Code's initial startup phase, goes away after
- **Fast VT response had ZERO effect on Claude Code startup stalls**
- **This DEFINITIVELY ELIMINATES VT response round-trip latency as the cause**
- The pty_reader thread responds in <1ms but ConPTY still stalls output for 1-3 seconds

**Session 11 — Strip VT queries + filter DA responses:**
- Added `strip_vt_queries()`: removes ESC[c, ESC[>c, ESC[6n, ESC[?6n from ConPTY output
  before forwarding to stdout (prevents outer terminal from seeing queries)
- Added `filter_da_responses()`: removes ESC[?...c and ESC[>...c from stdin input
  (prevents rich outer terminal capabilities from reaching Claude Code)
- Build succeeded, tested with system conhost (conpty.dll still renamed to .bak)
- **Result: stalls PERSIST.** No `[strip]` or `[filter]` log entries appeared because
  system conhost with flags=0 doesn't send DA1 queries — the filtering was moot.
- Claude Code still uses mode 2026 regardless of DA1 response — it detects terminal
  capabilities through OTHER means (env vars, DECRPM queries, or unconditional use)
- **DA response filtering theory DISPROVEN**

**Session 12 — minimal-conpty → tmux → Claude Code:**
- Ran tmux INSIDE minimal-conpty, then launched Claude Code inside tmux
- **Result: NO STALLS.** Input and visual feedback are responsive, identical to tmux alone.
- The outer ConPTY output contains **ZERO mode 2026 sequences** (confirmed by grep)
- This proves tmux absorbs/intercepts mode 2026 and never forwards it
- tmux handles mode 2026 in its VT parser: `input.c:2035` captures ESC[?2026h
  → `screen_write_start_sync()`, and `input.c:1928` captures ESC[?2026l →
  `screen_write_stop_sync()`. tmux re-renders to the outer terminal WITHOUT
  mode 2026, so the outer ConPTY never buffers.
- tmux also sets `TERM_PROGRAM=tmux` in child env — tested separately in Session 13.

**Session 13 — TERM_PROGRAM=tmux in child environment:**
- Set `TERM_PROGRAM=tmux`, `TERM_PROGRAM_VERSION=3.6a`, `TMUX=/tmp/tmux-fake,99999,0`
  in minimal-conpty's child environment
- **Result: stalls PERSIST (1.7-2.3s).** Claude Code uses mode 2026 UNCONDITIONALLY
  regardless of TERM_PROGRAM.
- Log shows ESC[?2026h ESC[?2026l at every stall boundary:
  - read #17: **1705ms** stall → `ESC[?2026h ESC[?2026l` → 2057 bytes render data
  - read #52: **2327ms** stall → `ESC[?2026h ESC[?2026l`
- **CRITICAL STALL PATTERN:** ReadFile blocks for 1.7-2.3s, then returns the h+l markers
  together in one 16-byte read, immediately followed by buffered render data. This
  confirms conhost buffers its entire VT output pipe during the sync update.
- **TERM_PROGRAM theory DISPROVEN.** The env var doesn't affect mode 2026 usage.
- **Claude Code enables mode 2026 unconditionally** — no env var or capability check.

### Key insight: pre-sent DSR is wrong with flags=0

With flags=0x3 (INHERIT_CURSOR), conhost sends ESC[6n and our pre-sent ESC[1;1R
is a valid response. With flags=0, conhost does NOT send ESC[6n, so the pre-sent
ESC[1;1R is an unsolicited CPR that confuses conhost, causing the 5.8s startup stall
and duplicate screen repaints.

OpenConsole (sideloaded) sends ESC[6n regardless of flags, so the pre-sent response
is valid with sideloaded OpenConsole even with flags=0.

### Key insight: stall is ONLY during Claude Code startup, not CMD prompt

Across all 8 sessions, CMD prompt echo is fast (1-50ms) once sideloaded ConPTY
and flags=0 are used. The 1.5-3s stalls ONLY appear during Claude Code's startup
phase — they go away after Claude Code finishes initializing.

Claude Code sets the console to raw mode (no conhost echo) — all echo depends on
Claude Code reading stdin and writing stdout. During startup, Claude Code is doing
heavy initialization (loading node.js, npm packages, etc.) and cannot promptly
read stdin or produce stdout. ConPTY output is batched/stalled during this phase.

## Deep comparison: tmux vs zellij ConPTY handling

### What tmux does (c:\src\tmux\win32\win32-pty.c)
- `CreatePipe(NULL, 0)` for both pipes (default ~4KB)
- `CreatePseudoConsole(size, pipeIn_read, pipeOut_write, 0, &hpc)` — flags=0
- Two bridge threads: ReadFile→send(socket), recv(socket)→WriteFile
- Bridge socket is a `win32_socketpair()` — full-duplex
- Main thread: libevent with bufferevent on the bridge socket
- Responds to DA1 (ESC[c) with **`ESC[?1;2c`** (VT100 with AVO)
- Responds to DSR (ESC[6n) with cursor position report
- Responds to DECRPM mode queries
- Does NOT send any initial sequences to ConPTY input
- Creates ConPTY at the correct pane size (no immediate resize)
- **Sets child environment: `TERM_PROGRAM=tmux`, `TERM=xterm-256color`,
  `TMUX=/path,pid,idx`, `COLORTERM=truecolor`** (`environ.c:264-282`)
- **Handles mode 2026 (synchronized output) internally** — `input.c:2035/1928`
  absorbs ESC[?2026h/l in VT parser, never forwards to outer terminal

### What zellij does
- `CreatePipe` with default for stdin, was 1MB for stdout (now 4KB)
- `CreatePseudoConsole(size, ...)` with flags=0 (was 0x3)
- pty_reader thread → tokio::sync::mpsc::channel(64) → async reader
- pty_writer thread ← bus::recv() ← Screen thread
- Responds to DA1 (ESC[c) with **`ESC[?1;2c`** (VT100 with AVO, changed to match tmux)
- Responds to DSR (ESC[6n) with cursor position report
- Was pre-sending DSR response (now removed)

### ~~Critical difference: DA1 response~~ (DISPROVEN — Session 6)

| | tmux | zellij |
|--|------|--------|
| **DA1 response** | `ESC[?1;2c` (VT100+AVO) | `ESC[?1;2c` (VT100+AVO, changed to match tmux) |

~~ConPTY/OpenConsole uses the DA1 response to configure its VT processing.~~
**Session 6 confirmed:** changing DA1 from VT220+sixel to VT100+AVO had zero
effect. Stalls identical (2.3s vs 2.4s). DA1 response is NOT the cause.

### ~~Remaining critical difference: I/O bridge architecture~~ (DISPROVEN — Session 7)

~~tmux uses socketpair bridge threads (ReadFile→send(socket)→libevent).~~
~~zellij uses tokio mpsc channel (ReadFile→blocking_send(channel)→async recv).~~
**Session 7 confirmed:** replacing mpsc channel with TCP socketpair (matching tmux
exactly) had zero effect. Stalls identical (0.5-2.7s). The bridge architecture is NOT
the cause. All ConPTY-level differences between tmux and zellij have been tested
and eliminated.

### ROOT CAUSE: Mode 2026 (Synchronized Output) + ConPTY output buffering

**IDENTIFIED in Sessions 9-12.** The stall mechanism:

1. Claude Code (ink/React) wraps renders in ESC[?2026h ... ESC[?2026l
2. ConPTY's conhost recognizes mode 2026 and BUFFERS its VT output pipe
3. During Claude Code startup, synchronized updates last 2-4 seconds
4. ReadFile blocks for the entire duration — conhost holds output until ESC[?2026l
5. All buffered output flushes at once when the sync update ends

**Why tmux doesn't stall** (Session 12 proved this definitively):
- tmux runs Claude Code in an INNER ConPTY
- tmux's VT parser absorbs ESC[?2026h/l internally (`screen_write_start_sync`)
- tmux re-renders to the OUTER ConPTY WITHOUT mode 2026 sequences
- Outer ConPTY never sees mode 2026 → never buffers → no stall
- Additionally, tmux sets `TERM_PROGRAM=tmux` in child environment, which may
  cause Claude Code to disable mode 2026 entirely

**Two potential fixes for zellij:**

**Fix A (environment):** Set `TERM_PROGRAM=zellij` (or `tmux`) in the ConPTY
child environment. If Claude Code checks `TERM_PROGRAM` and disables mode 2026
for tmux, it may do the same for zellij (or we fake being tmux).

**Fix B (VT parser):** zellij already parses VT output from ConPTY. Add handling
for mode 2026 in zellij's grid/VT parser to absorb it, matching tmux. This is
the correct long-term fix since zellij IS a terminal multiplexer and should
handle synchronized output internally, just like tmux does.

Possible remaining causes (investigated below):

### Cause 1: Process tree / job object differences — ELIMINATED

Both zellij and tmux use identical CreateProcess configuration for ConPTY panes:
- Same flags: `EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT`
- Same attribute: `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`
- Same handle inheritance: `bInheritHandles = FALSE`
- Same std handle setup: `INVALID_HANDLE_VALUE` with `STARTF_USESTDHANDLES`
- Neither uses Job Objects
- Zellij: `portable-pty-patch/src/win/psuedocon.rs:127-188`
- Tmux: `win32/win32-pty.c:116-159`
**Verdict: NOT the cause.** Process creation is identical.

### Cause 2: Console mode / input handling — ELIMINATED

Both write to the ConPTY input pipe synchronously:
- Zellij: pty_writer thread calls `writer.write(buf)` + `writer.flush()` per keystroke
- Tmux: pty_input_thread calls `WriteFile(hPipeIn, buf, n)` per recv()
- Zellij actually flushes MORE aggressively (explicit tcdrain after every write)
- Logs confirm write+drain never exceeds 100ms (threshold never triggered)
- Keystrokes reach ConPTY pipe immediately in both (pty_writer logs confirm)
**Verdict: NOT the cause.** Input path is at least as fast as tmux.

### Cause 3: Tokio runtime resource contention — ELIMINATED

- The ReadFile stall occurs on the pty_reader OS thread, completely outside tokio
- tokio runtime has 4 worker threads (`global_async_runtime.rs`)
- Screen thread and PTY thread run on separate OS threads, NOT on tokio
- TerminalBytes::listen() is a lightweight async task (waits on I/O)
- spawn_blocking for send_to_screen just does a crossbeam channel send (fast)
- No evidence of any blocking operation on tokio that could affect ConPTY
**Verdict: NOT the cause.** ReadFile stall is independent of tokio scheduling.

### Cause 4: VT response round-trip latency — ELIMINATED (Session 8)

**Previously the most promising lead, now definitively disproven.**
Fast VT response path responds in <1ms (matching tmux) but stalls are identical.
ConPTY output stalls are NOT caused by waiting for VT query responses.

Original analysis (for reference):

**Zellij's VT response path (15 hops):**
```
ConPTY output pipe (query: ESC[c)
  → pty_reader thread: ReadFile() → write(socket)
  → TCP socketpair
  → tokio async TcpStream::read()
  → terminal_bytes::listen() → spawn_blocking(send_to_screen)
  → tokio blocking thread pool → crossbeam channel
  → Screen thread: ScreenInstruction::PtyBytes
  → tab.handle_pty_bytes(pid, bytes)
  → terminal_output.handle_pty_bytes(bytes) — VT parse
  → grid.csi_dispatch() — detects ESC[c, pushes response to pending_messages_to_pty
  → drain_messages_to_pty() — after parsing completes
  → write_to_pane_id_without_preprocessing()
  → send_to_pty_writer(PtyWriteInstruction::Write)
  → pty_writer thread: bus.recv() → write_to_tty_stdin() + tcdrain()
  → ConPTY input pipe (response: ESC[?1;2c)
```
**Minimum 3 thread hops** (pty_reader → tokio → screen → pty_writer)
**Estimated round-trip: 7-100+ ms**

**Tmux's VT response path (direct, no thread hops):**
```
ConPTY output pipe (query: ESC[c)
  → pty_bridge_thread: ReadFile() → send(socket)
  → TCP socketpair
  → libevent bufferevent callback (main thread)
  → input_process() → input_csi_dispatch()
  → input_reply(ictx, "\033[?1;2c") — IMMEDIATE bufferevent_write()
  → socket → pty_input_thread: recv() → WriteFile()
  → ConPTY input pipe
```
**Zero extra thread hops** — response is generated in the same callback that
parses the query and written IMMEDIATELY via bufferevent.
**Estimated round-trip: 1-2 ms**

**Why this matters for ConPTY stalls:**
OpenConsole.exe may BLOCK output production while waiting for a VT response.
The ConPTY host sends queries (DA1, DSR, mode queries) and needs responses
to configure its VT output pipeline. If the response takes 10-100ms instead
of 1-2ms, OpenConsole's output thread could be stalled waiting.

During Claude Code operation specifically, Claude Code generates output that
triggers ConPTY to send additional queries (cursor position, terminal size,
etc.). Each query→response adds latency. With rapid typing, multiple queries
can stack up, each adding 10-100ms of delay, compounding to 2+ seconds.

**The pty_writer thread is SHARED** between user keystrokes and VT responses.
If a keystroke write + tcdrain is in progress, the VT response is queued
BEHIND it, adding further delay.

## Options to try next

### Option A: Match tmux's DA1 response ← TESTED, NO EFFECT

**What:** Change `zellij-server/src/panes/grid.rs:3357` from `ESC[?62;4c`
(VT220 with sixel) to `ESC[?1;2c` (VT100 with AVO) to match tmux.

**Why it might help:** The DA1 response flows to TWO consumers:
1. **OpenConsole/conhost** — uses it to configure VT output processing. VT220+sixel
   may enable more complex escape sequence generation (sixel graphics support,
   8-bit controls, etc.) which could slow output production.
2. **Claude Code (Node.js/ink)** — queries terminal capabilities via DA1. VT220+sixel
   may cause it to use heavier rendering (more escape sequences per frame, sixel
   graphics probing). VT100 would force simpler output.

**Risk:** Low. Other terminal programs might lose sixel support, but Claude Code
and most CLI tools don't use sixel. If needed, we can make this Windows-only.

**Files:** `zellij-server/src/panes/grid.rs:3357` — one line change.

### Option B: Minimal ConPTY test program (most diagnostic)

**What:** Write a ~100 line C program that creates a ConPTY with the exact same
configuration as tmux:
```c
CreatePipe(NULL, 0) for both pipes       // 4KB default
CreatePseudoConsole(size, in, out, 0)    // flags=0
// Two bridge threads: ReadFile→WriteFile(stdout), ReadFile(stdin)→WriteFile
// Respond to ESC[c with ESC[?1;2c
```
Then run `claude` inside it and type rapidly. This eliminates ALL variables
except "is it ConPTY/Claude Code or is it zellij?"

**Why it's valuable:** If the test program has stalls, the problem is fundamentally
in ConPTY or Claude Code's interaction with it — nothing zellij can fix at the
ConPTY level, and we'd need to look at workarounds (local echo, etc.). If the
test program is responsive, something in zellij's process causes the issue and
we narrow the search to architecture/scheduling/resource contention.

**Effort:** ~1-2 hours to write and test. Can be placed in `tools/conpty_test.c`.

### Option C: Stop responding to ConPTY queries

**What:** Comment out the DA1 response (`ESC[?62;4c`) and DSR response in
`grid.rs` so zellij sends NOTHING back to ConPTY when queried. OpenConsole
will timeout waiting for responses.

**Why:** Isolates whether ANY response to ConPTY (not just the specific value)
affects behavior. If stalls disappear, the response mechanism itself is the
issue. If stalls persist, responses are irrelevant.

**Risk:** Medium. OpenConsole may behave differently without responses (longer
startup, different VT mode). tmux DOES respond, so this diverges from tmux.

### Option D: Socket bridge (match tmux architecture exactly) ← TESTED, NO EFFECT

**What:** Replace zellij's ConPTY I/O bridge:
```
Current:  pty_reader thread → tokio::mpsc::channel(64) → async reader → terminal_bytes
Proposed: pty_reader thread → send(socketpair) → [main thread reads socket]
```
Use `win32_socketpair()` (or `WSASocket` + `bind/connect` on localhost) to
create the same bridge topology as tmux.

**Why:** The Windows kernel handles socket I/O differently from inter-thread
channels. Sockets use Winsock (AFD driver), channels use userspace atomics.
The scheduling behavior could be different — sockets may wake the reading
thread faster, or provide different backpressure characteristics.

tmux's exact path: `ReadFile(pipe) → send(socket) → libevent(socket) → parse`.
zellij's path: `ReadFile(pipe) → blocking_send(channel) → recv(channel) → parse`.

**Effort:** Medium (~4 hours). Need to create socketpair, replace channel with
socket reads in the async reader, keep the rest of the pipeline unchanged.

**Risk:** Low. The socket bridge is a drop-in replacement for the channel.

### Option E: ETW kernel tracing (definitive root cause)

**What:** Use Windows Performance Toolkit (WPR/WPA) or `xperf` to capture
kernel-level I/O traces during a zellij session and a tmux session. Compare
the ConPTY pipe ReadFile/WriteFile timing, thread scheduling, and lock
contention between the two.

**How:**
```powershell
# Start trace
xperf -on PROC_THREAD+LOADER+FILE_IO+FILE_IO_INIT -stackwalk FileRead+FileWrite
# Run zellij, reproduce stall
# Stop trace
xperf -d zellij_trace.etl
# Open in WPA, filter to conhost.exe/OpenConsole.exe, compare with tmux trace
```

**Why:** This gives DEFINITIVE evidence of what ConPTY is doing during the stall.
We can see: is conhost blocked on a lock? Is it doing CPU work? Is it waiting
for I/O? Is the thread descheduled? No more guessing.

**Effort:** Medium (~2-3 hours including analysis). Requires Windows Performance
Toolkit installed (`winget install Microsoft.WindowsPerformanceToolkit`).

### Option F: Check for ConPTY resize at spawn

**What:** Add logging to verify whether zellij resizes the ConPTY immediately
after creation. Check `PsuedoCon::resize()` call timing relative to spawn.

**Why:** tmux creates ConPTY at the exact pane size. If zellij creates at a
default size (e.g., 80x24) then immediately resizes to the actual pane size,
the resize triggers conhost to repaint the screen buffer. Without RESIZE_QUIRK
(flags=0), the resize still happens but may cause a different internal codepath
that stalls output. An early resize during cmd.exe initialization could confuse
conhost's output state machine.

**How to check:** Grep for `resize` calls in `conpty.rs` and add timestamp
logging. Check the zellij log for any resize operations in the first second.

**Effort:** Low (~30 minutes). Mostly investigation, minimal code change.

### Option G: Fast VT response path (bypass pty_writer) ← TESTED, NO EFFECT (Session 8)

**What:** Intercept VT queries (ESC[c, ESC[6n, etc.) in the pty_reader bridge
thread or terminal_bytes layer and respond IMMEDIATELY, writing the response
directly to the ConPTY input pipe — bypassing the entire screen → grid →
pty_writer round-trip. This matches tmux's architecture where the response
is generated inline during parsing.

**Implementation:**
```
Current:  ConPTY query → pty_reader → async → screen → grid → pending_messages
          → pty_writer → ConPTY input (15 hops, 7-100+ms)
Proposed: ConPTY query → pty_reader → DETECT & RESPOND IMMEDIATELY → ConPTY input
          (also forward data to screen for normal processing)
```

In the pty_reader bridge thread (`os_input_output_windows.rs`), after ReadFile:
1. Scan the output for known VT queries (ESC[c, ESC[6n)
2. If found, write the response directly to the ConPTY input pipe
3. Forward the data to the rest of the pipeline as normal

The pty_reader already has the ConPTY output data in a buffer. We need a
second handle (or arc) to the ConPTY input pipe writer to send responses
directly from this thread.

**Why it's the strongest lead:**
- VT response round-trip is the ONLY remaining architectural difference
  between tmux and zellij that hasn't been tested
- tmux responds in 1-2ms; zellij takes 7-100ms+ due to 3 thread hops
- ConPTY is known to stall output waiting for responses
- All 6 previous experiments eliminated ConPTY configuration as the cause;
  this is the first experiment targeting zellij's processing pipeline

**Risk:** Medium. Need to parse VT sequences in the bridge thread (simple
pattern match, not full parse). The response must be correct. Grid will
also generate a duplicate response (which should be harmless — ConPTY
ignores unsolicited responses).

**Effort:** Medium (~3-4 hours). Need to:
1. Share the ConPTY input pipe writer with the pty_reader thread
2. Add simple VT query pattern matching in the bridge thread
3. Write responses directly from the bridge thread
4. Keep forwarding data to async pipeline as before

**Files:**
- `zellij-server/src/os_input_output_windows.rs` — share writer, add response logic
- `portable-pty-patch/src/win/conpty.rs` — may need to expose writer clone

### Option H: Minimal ConPTY test program ← TESTED, STALLS WITH SIDELOADED CONPTY

Session 9 (16:27): Built `tools/minimal-conpty` — uses same portable-pty code
as zellij but with NO tokio, NO async, NO plugins, NO screen thread. Just two
raw OS threads bridging stdin↔ConPTY.

**Result: stalls persist with sideloaded conpty.dll (1.6-2.5s during Claude Code).**
This eliminates zellij's architecture as the cause. The stalls are in portable-pty
or the sideloaded ConPTY layer itself.

**CRITICAL DISCOVERY:** tmux does NOT have sideloaded conpty.dll/OpenConsole.exe.
tmux uses system kernel32 ConPTY (system conhost.exe). Our program uses sideloaded
OpenConsole.exe v1.23. We never cleanly tested system kernel32 + flags=0 + no
pre-sent DSR (Session 4 had the DSR bug).

**Session 10: renamed conpty.dll/OpenConsole.exe → .bak, forcing fallback
to system kernel32 ConPTY. STALLS PERSISTED (2.4-6.3s).** Eliminates
sideloaded vs system conpty as the cause.

**BREAKTHROUGH — Synchronized Output (Mode 2026) Theory:**

Every major stall in Sessions 9-10 ends with `ESC[?2026h ESC[?2026l` in the
ConPTY output. Mode 2026 is "synchronized output" — the terminal buffers
screen updates between h (begin) and l (end). ConPTY's conhost ALSO honors
this by buffering its VT output pipe during a synchronized update.

Claude Code (ink/React framework) uses synchronized updates to batch renders.
During startup, it may keep a sync update open for 2+ seconds while
initializing (loading node.js, npm modules, etc.).

**Why tmux doesn't stall:** tmux intercepts ConPTY's DA1 query (ESC[c) and
responds with ESC[?1;2c (VT100). The query NEVER reaches the outer terminal.
Claude Code inside tmux thinks it's in a VT100 — which does NOT support
mode 2026 — so it doesn't use synchronized updates.

In our program (and zellij), the DA1 query reaches the outer terminal
(Windows Terminal), which responds with rich VT500+ capabilities. Claude Code
sees these and enables synchronized updates, causing ConPTY output buffering.

**Session 11 test: strip VT queries from output (prevent outer terminal from
seeing them) AND filter DA responses from input (prevent rich capabilities
from reaching Claude Code). This matches tmux's query interception.**

Previous description (for reference):
Now the MOST important experiment. All 8 sessions have eliminated every
zellij-side variable. A ~100 line C/Rust program with identical ConPTY
config will answer the definitive question:

- **If minimal program has same stalls:** The problem is fundamental to
  ConPTY + Claude Code startup interaction. Nothing zellij can fix at the
  ConPTY level. Would need workarounds like local echo prediction.
- **If minimal program is responsive:** Something about zellij's process
  environment (thread count, memory, CPU contention from plugin/wasm threads)
  affects ConPTY performance, and we can narrow the search.

### Option I: Local echo prediction (workaround, skip ConPTY round-trip)

If Option H confirms stalls are inherent to ConPTY, the only fix is to
predict and display keystrokes locally without waiting for ConPTY output.
This is what SSH clients (mosh) do for high-latency connections.

**Implementation:** When a keystroke is sent to pty_writer, immediately
render it on screen at the cursor position. When ConPTY output arrives,
reconcile the predicted echo with actual output. Complex for cursor-moving
programs but simple for basic typing.

### Option J: Verify tmux comparison is valid ← TESTED, tmux IS fast

User confirmed: Claude Code inside tmux on Windows is fast and responsive,
just like standard cmd.exe and Windows Terminal. The stalls are zellij-specific.

**Session 12 (KEY TEST):** minimal-conpty → tmux → Claude Code = NO STALLS.
This proves tmux actively prevents the stalls (not just "happens to work").
The outer ConPTY is IDENTICAL (same portable-pty, same config), yet adding
tmux as an intermediary eliminates stalls completely.

### Option K: Set TERM_PROGRAM in child environment ← TESTED, NO EFFECT (Session 13)

Set `TERM_PROGRAM=tmux`, `TMUX=fake` in child env. Claude Code uses mode 2026
unconditionally — env vars have no effect. Stalls identical (1.7-2.3s).

### Option L: Handle mode 2026 in zellij's VT parser

**What:** Zellij already parses VT output in its grid. The issue is that the
parsing never happens during a stall because conhost buffers the output pipe.
Zellij can't absorb mode 2026 because it never sees the data until conhost
flushes. This option is NOT viable for the ConPTY stall — it would only
help if we had a dual ConPTY architecture like tmux.

### Option M: PSEUDOCONSOLE_PASSTHROUGH flag (Windows 11 24H2+)

**What:** Use `CreatePseudoConsole` with `PSEUDOCONSOLE_PASSTHROUGH = 0x8`.
This tells conhost to pass through VT sequences WITHOUT processing them.
Mode 2026 would not be recognized → no buffering.

**Trade-off:** In passthrough mode, zellij must do ALL VT processing itself
(conhost doesn't render the virtual console). This is a major architectural
change but aligns with how tmux works (tmux is the terminal emulator).

**Availability:** Only in Windows 11 24H2+ (Insider builds). Not available
on Build 26200. Future option only.

### Option N: Write ESC[?2026l periodically to ConPTY input pipe

**What:** Detect ESC[?2026h in the output and immediately write ESC[?2026l
to the input pipe, forcing conhost to end the sync update early.

**Problem:** The input pipe goes to the child's stdin, not to conhost's VT
parser. Writing ESC[?2026l to the input pipe doesn't reset mode 2026 in
conhost — it becomes stdin data for Claude Code. NOT viable.

### Option O: Respond "not supported" to DECRPM query for mode 2026

**What:** If conhost passes ESC[?2026$p (DECRPM query) to the output pipe,
intercept it and respond ESC[?2026;0$y (not recognized).

**Problem:** Conhost likely handles DECRPM internally (it recognizes mode
2026). The query never reaches the output pipe. Also, Session 13 showed
Claude Code doesn't check DECRPM before enabling mode 2026 — it just
sends ESC[?2026h unconditionally. NOT viable.

### Option P: Dual ConPTY (wrapper process)

**What:** Instead of running Claude Code directly in a ConPTY, run a thin
wrapper process that creates a SECOND ConPTY for the actual child. The
wrapper reads the inner ConPTY output, strips mode 2026, and writes to
its own stdout. This mimics tmux's dual-ConPTY architecture.

**Trade-off:** Complex, adds latency, doubles ConPTY overhead per pane.
But it would definitively fix mode 2026 buffering.

### Option Q: Upstream fix — Claude Code respects NO_SYNC_OUTPUT env var

**What:** File an issue with Claude Code / ink requesting an env var to
disable synchronized output (mode 2026). Example: `NO_SYNC_OUTPUT=1`.
Terminal multiplexers (zellij, screen) could set this.

**Why:** This is the correct long-term fix. Terminal multiplexers handle
sync output internally. Children shouldn't use mode 2026 when running
inside a mux that already manages screen updates.

## Summary of all experiments (13 sessions)

| # | Session | Variable tested | Result |
|---|---------|----------------|--------|
| 1 | 12:23 | flags=0x3 + system conhost | 1.2-2.6s stalls |
| 2 | 12:47 | flags=0x3 + sideloaded OpenConsole | 2.3-4.0s stalls |
| 3 | 13:03 | flags=0 + sideloaded | CMD fast, Claude stalls 2.4-2.6s |
| 4 | 13:42 | flags=0 + system conhost | 5.8s startup stall (pre-sent DSR bug) |
| 5 | 13:59 | 4KB pipe + sideloaded + flags=0 | CMD fast, Claude stalls 2.4s |
| 6 | 14:42 | DA1→VT100+AVO | Claude stalls 2.3s (identical) |
| 7 | 15:09 | TCP socketpair bridge | Claude stalls 0.5-2.7s (identical) |
| 8 | 15:51 | Fast VT response (<1ms) | Claude stalls 1.6-2.4s (identical) |
| 9 | 16:27 | Minimal ConPTY (no zellij) | Stalls 1.6-2.5s (eliminates zellij) |
| 10 | — | System kernel32 (no sideloading) | Stalls 2.4-6.3s (eliminates sideloading) |
| 11 | — | Strip VT queries + filter DA | Stalls persist (filtering was moot) |
| 12 | — | minimal-conpty → tmux → Claude | **NO STALLS** — dual ConPTY absorbs mode 2026 |
| 13 | — | TERM_PROGRAM=tmux env var | Stalls 1.7-2.3s — mode 2026 is unconditional |
| 14 | — | tmux ReadFile timing instrumentation | tmux has IDENTICAL stalls (up to 4091ms) |
| 15 | — | grid.rs mode 2026 safety timeout (1s) | Pane unlocks, but ReadFile stall unchanged |
| 16 | 18:47 | Strip mode 2026 in pty_reader | 42 pairs stripped; ReadFile stalls unchanged (667ms-2800ms) |

**ROOT CAUSE CONFIRMED: Synchronized Output (Mode 2026) + ConPTY output buffering.**

Claude Code uses ESC[?2026h/l (synchronized output) UNCONDITIONALLY to batch
renders. It does not check TERM_PROGRAM, TMUX, or any env var — Session 13
proved this. ConPTY's conhost honors mode 2026 by buffering its VT output pipe
until ESC[?2026l arrives. During Claude Code's startup, single sync updates
last 1.7-2.3 seconds, causing the ReadFile stall.

**Stall pattern (confirmed in Session 13):**
```
T+0.0s: Claude Code sends ESC[?2026h (begin sync update)
T+0.0s: Conhost starts buffering VT output pipe
T+0.0s to T+1.7s: Claude Code initializes, updates virtual console
T+1.7s: Claude Code sends ESC[?2026l (end sync update)
T+1.7s: Conhost flushes: [ESC[?2026h ESC[?2026l] + [2057 bytes render data]
T+1.7s: ReadFile returns with all buffered data at once
```

**Why tmux fixes it (dual ConPTY architecture):**
tmux creates an INNER ConPTY for each pane. Claude Code runs in the inner ConPTY:
1. Claude Code sends ESC[?2026h → inner conhost buffers → inner ReadFile stalls
2. tmux's reader thread for that pane waits (on separate thread)
3. tmux's main loop keeps running — renders status bar, handles input
4. tmux re-renders to the OUTER ConPTY WITHOUT mode 2026
5. Outer ConPTY never sees mode 2026 → never buffers → user sees responsive terminal
6. After 1.7s, inner ConPTY flushes → tmux re-renders the pane

**Implications for zellij:**
Zellij has a similar architecture to tmux — each pane has its own ConPTY with a
separate pty_reader thread. The ConPTY stall blocks only that pane's pty_reader.
Zellij's rendering (via crossterm to client terminal) should continue working
for other panes and UI elements. The stall should only affect keystroke echo
in the Claude Code pane.

**However:** Since zellij uses a SINGLE ConPTY per pane (not dual like tmux),
the conhost for that pane DOES buffer for mode 2026. The only way to eliminate
this is to prevent conhost from seeing mode 2026.

## Sessions 14-16: Confirming ConPTY stall is a platform limitation

### Session 14: tmux ReadFile timing instrumentation

Added timing instrumentation to tmux's `pty_bridge_thread` in `win32/win32-pty.c`.
User typed "alalalalal" repeatedly in tmux → Claude Code with **no perceived delay**.

**Result:** tmux has IDENTICAL ReadFile stalls — up to **4091ms** at read #151.
Mode 2026 h+l always arrive together as 16-byte pairs, same as zellij. During
interactive typing, echoes were 47-742ms per keystroke. The stall is purely in
ConPTY's ReadFile blocking, not in any terminal multiplexer logic.

**Conclusion:** tmux's perceived responsiveness is NOT because it avoids the
stall. Both tmux and zellij have the same ConPTY ReadFile blocks. The difference
in user perception may be due to different testing conditions (typing during
different phases of Claude Code's render cycle).

### Session 15: grid.rs mode 2026 safety timeout

Added a 1-second safety timeout for `lock_renders` in `grid.rs`, matching
tmux's `MODE_SYNC` timeout behavior:
- `lock_renders_at: Option<std::time::Instant>` field
- `render()` auto-unlocks after 1 second
- `is_mid_frame()` respects timeout

**Result:** Prevents zellij from indefinitely locking renders if ESC[?2026l
never arrives. However, since h+l always arrive together in ConPTY, this timeout
never actually triggers in practice. ReadFile stalls unchanged.

### Session 16: Strip mode 2026 in pty_reader (18:47)

Added `strip_mode_2026()` function in `os_input_output_windows.rs` that removes
ESC[?2026h and ESC[?2026l from ConPTY output before forwarding to the pipeline.
This matches tmux's MODE_SYNC absorption behavior.

**Result:**
- 84 strip operations (42 h/l pairs) — stripping works correctly
- ConPTY ReadFile stalls persist: 667ms-2800ms during Claude Code interaction
- Per-keystroke echo during Claude Code's raw mode: 667-2380ms
- The stalls happen INSIDE conhost before data reaches ReadFile

**Timeline (18:47 session):**
```
18:47:15.464  read #1: cmd.exe startup (fast, 5ms)
18:47:16.186  read #4: first keystroke echo 'c' (702ms — waiting for user)
18:47:17.717  read #13: first mode 2026 pair (115ms) → STRIPPED
18:47:18.549  read #17: mode 2026 pair (823ms stall) → STRIPPED
18:47:18.549  read #18: 2023 bytes content (0ms — immediate after strip)
18:47:21.990  read #43: mode 2026 pair (2321ms stall) → STRIPPED
18:47:26.985  read #122: mode 2026 pair (2800ms stall) → STRIPPED
18:47:28-42   reads #138-#8520: interactive echoes, 667-2380ms per keystroke
```

**Why stripping doesn't help perceived performance:**
The stall happens inside ConPTY's conhost, which buffers all output between
ESC[?2026h and ESC[?2026l. Our stripping happens AFTER ReadFile returns — we
can't prevent conhost from buffering. The data flow:

```
Claude Code stdout → conhost (buffers during mode 2026) → output pipe → ReadFile
                     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                     THIS is where the stall happens — we can't touch it
```

Stripping prevents grid.rs from calling lock_renders(), which is a correctness
improvement, but doesn't affect the ConPTY-level buffering.

## Final conclusion: ConPTY mode 2026 stall is unfixable at the app layer

After 16 sessions of investigation:

1. **Root cause:** ConPTY's conhost buffers VT output during mode 2026
   (synchronized output). Claude Code's Ink framework sends mode 2026
   unconditionally on every render cycle.

2. **tmux has the same stalls.** Instrumented timing proves ReadFile blocks
   up to 4091ms in tmux. The perceived difference is likely due to testing
   methodology, not architecture.

3. **Nothing at the zellij/tmux layer can prevent the stall.** The buffering
   happens inside conhost before data reaches the output pipe. Stripping mode
   2026 after ReadFile, adding safety timeouts, fast VT responses, pipe buffer
   tuning, sideloaded ConPTY — none of these change the fundamental behavior.

4. **The stall only affects the specific pane.** Other panes and zellij's UI
   remain responsive. This is the same behavior as tmux.

5. **Potential fixes require changes outside zellij:**
   - Claude Code/Ink: disable mode 2026 on Windows ConPTY
   - Microsoft: add an option to disable mode 2026 in conhost
   - A dual ConPTY relay that strips mode 2026 between inner and outer
     (proven in Session 12 but adds complexity)

## Additional finding: Ctrl+C during startup lag kills Claude Code

When Ctrl+C is pressed during initialization, Claude Code is killed. Our
PeekConsoleInput helper waits 100ms then checks if 0x03 was consumed. During
startup, node.js hasn't started its stdin read loop yet, so the 0x03 sits
unconsumed → helper exits 42 → `terminate_descendants` kills Claude Code.

## Known upstream issues
- [anthropics/claude-code#7694](https://github.com/anthropics/claude-code/issues/7694) — Claude Code Windows input lag
- [microsoft/terminal#11794](https://github.com/microsoft/terminal/issues/11794) — conhost hangs on large output
- [microsoft/terminal#10362](https://github.com/microsoft/terminal/issues/10362) — slow VT sequence processing
- [microsoft/terminal#262](https://github.com/microsoft/terminal/issues/262) — no overlapped I/O in ConPTY
