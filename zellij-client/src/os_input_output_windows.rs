use crate::os_input_output::SignalEvent;

use async_trait::async_trait;

use std::io;
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::Duration;

/// Flag set by the console ctrl handler when Ctrl-C is pressed.
/// On Windows, Ctrl-C should be forwarded to the active terminal pane
/// (as byte 0x03) rather than treated as a quit signal. The stdin read
/// loop checks this flag to synthesize the byte when ReadFile is
/// interrupted by the CTRL_C_EVENT.
pub static CTRL_C_PRESSED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Async signal listener for Windows.
///
/// Uses `tokio::signal::windows` for Ctrl-C / Ctrl-Break, and polls
/// `crossterm::terminal::size()` for resize detection.
pub(crate) struct AsyncSignalListener {
    ctrl_c: tokio::signal::windows::CtrlC,
    ctrl_break: tokio::signal::windows::CtrlBreak,
    resize_rx: tokio::sync::mpsc::Receiver<()>,
}

impl AsyncSignalListener {
    pub fn new() -> io::Result<Self> {
        let ctrl_c = tokio::signal::windows::ctrl_c()?;
        let ctrl_break = tokio::signal::windows::ctrl_break()?;

        let (resize_tx, resize_rx) = tokio::sync::mpsc::channel(16);

        // Spawn a background thread that polls terminal size for changes
        thread::Builder::new()
            .name("resize_poll".to_string())
            .spawn(move || {
                let mut last_size = crossterm::terminal::size().unwrap_or((80, 24));
                loop {
                    thread::sleep(Duration::from_millis(100));
                    match crossterm::terminal::size() {
                        Ok(new_size) if new_size != last_size => {
                            last_size = new_size;
                            if resize_tx.blocking_send(()).is_err() {
                                break; // receiver dropped
                            }
                        },
                        _ => {},
                    }
                }
            })
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        Ok(Self {
            ctrl_c,
            ctrl_break,
            resize_rx,
        })
    }
}

#[async_trait]
impl crate::os_input_output::AsyncSignals for AsyncSignalListener {
    async fn recv(&mut self) -> Option<SignalEvent> {
        tokio::select! {
            _result = self.ctrl_c.recv() => {
                // Ctrl-C should be forwarded to terminal panes, not trigger quit.
                // Set the flag so the stdin loop can synthesize byte 0x03.
                CTRL_C_PRESSED.store(true, std::sync::atomic::Ordering::SeqCst);
                // Return None to continue the select loop (no SignalEvent emitted)
                None
            },
            result = self.ctrl_break.recv() => result.map(|_| SignalEvent::Quit),
            result = self.resize_rx.recv() => result.map(|_| SignalEvent::Resize),
        }
    }
}

/// Blocking signal iterator for Windows.
///
/// Spawns a thread that uses `ctrlc`-style handling (via a raw
/// `SetConsoleCtrlHandler` wrapper) for quit signals, and polls
/// `crossterm::terminal::size()` for resize events.
pub(crate) struct BlockingSignalIterator {
    rx: std_mpsc::Receiver<SignalEvent>,
}

impl BlockingSignalIterator {
    pub fn new() -> io::Result<Self> {
        let (tx, rx) = std_mpsc::channel();

        // Thread for resize polling
        let resize_tx = tx.clone();
        thread::Builder::new()
            .name("blocking_resize_poll".to_string())
            .spawn(move || {
                let mut last_size = crossterm::terminal::size().unwrap_or((80, 24));
                loop {
                    thread::sleep(Duration::from_millis(100));
                    match crossterm::terminal::size() {
                        Ok(new_size) if new_size != last_size => {
                            last_size = new_size;
                            if resize_tx.send(SignalEvent::Resize).is_err() {
                                break;
                            }
                        },
                        _ => {},
                    }
                }
            })
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        // Thread for Ctrl-C / Ctrl-Break handling
        let quit_tx = tx;
        thread::Builder::new()
            .name("blocking_ctrl_handler".to_string())
            .spawn(move || {
                // Use a simple polling approach with tokio's blocking ctrl_c
                // since we need this on a blocking thread
                use windows_sys::Win32::System::Console::{
                    SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT,
                };

                // We use a static channel sender via a global, since
                // SetConsoleCtrlHandler requires a static function pointer.
                // For simplicity in this blocking context, we just use a
                // parking_lot-free approach: poll a flag set by the handler.
                static QUIT_FLAG: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);

                unsafe extern "system" fn handler(ctrl_type: u32) -> i32 {
                    match ctrl_type {
                        CTRL_C_EVENT => {
                            // Don't quit — forward Ctrl-C to the active terminal pane.
                            // Set CTRL_C_PRESSED so the stdin loop can synthesize byte 0x03
                            // (ReadFile is interrupted by CTRL_C_EVENT on Windows and won't
                            // deliver the byte itself).
                            CTRL_C_PRESSED.store(true, std::sync::atomic::Ordering::SeqCst);
                            1 // handled — prevent default termination
                        },
                        CTRL_BREAK_EVENT => {
                            QUIT_FLAG.store(true, std::sync::atomic::Ordering::SeqCst);
                            1 // handled
                        },
                        _ => 0,
                    }
                }

                unsafe {
                    SetConsoleCtrlHandler(Some(handler), 1);
                }

                loop {
                    thread::sleep(Duration::from_millis(50));
                    if QUIT_FLAG.load(std::sync::atomic::Ordering::SeqCst) {
                        QUIT_FLAG.store(false, std::sync::atomic::Ordering::SeqCst);
                        if quit_tx.send(SignalEvent::Quit).is_err() {
                            break;
                        }
                    }
                }
            })
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        Ok(Self { rx })
    }
}

impl Iterator for BlockingSignalIterator {
    type Item = SignalEvent;

    fn next(&mut self) -> Option<SignalEvent> {
        self.rx.recv().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn async_signal_listener_can_be_constructed() {
        let listener = AsyncSignalListener::new();
        assert!(listener.is_ok(), "AsyncSignalListener::new() should succeed: {:?}", listener.err());
    }

    #[test]
    fn blocking_signal_iterator_can_be_constructed() {
        let iter = BlockingSignalIterator::new();
        assert!(iter.is_ok(), "BlockingSignalIterator::new() should succeed: {:?}", iter.err());
    }
}
