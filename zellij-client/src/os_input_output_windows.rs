use crate::os_input_output::SignalEvent;

use async_trait::async_trait;

use std::io;
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::Duration;

/// Async signal listener for Windows.
///
/// Uses `tokio::signal::windows` for Ctrl-Break, and polls
/// `crossterm::terminal::size()` for resize detection.
///
/// Ctrl-C is NOT handled here — with ENABLE_PROCESSED_INPUT disabled
/// (raw console mode), byte 0x03 is delivered directly through ReadFile
/// to the stdin reader, which forwards it to the active terminal pane.
pub(crate) struct AsyncSignalListener {
    ctrl_break: tokio::signal::windows::CtrlBreak,
    resize_rx: tokio::sync::mpsc::Receiver<()>,
}

impl AsyncSignalListener {
    pub fn new() -> io::Result<Self> {
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
            ctrl_break,
            resize_rx,
        })
    }
}

#[async_trait]
impl crate::os_input_output::AsyncSignals for AsyncSignalListener {
    async fn recv(&mut self) -> Option<SignalEvent> {
        tokio::select! {
            result = self.ctrl_break.recv() => result.map(|_| SignalEvent::Quit),
            result = self.resize_rx.recv() => result.map(|_| SignalEvent::Resize),
        }
    }
}

/// Blocking signal iterator for Windows.
///
/// Spawns a thread that uses `SetConsoleCtrlHandler` for Ctrl-Break
/// (quit signal), and polls `crossterm::terminal::size()` for resize events.
///
/// Ctrl-C is NOT intercepted — it flows through ReadFile as byte 0x03
/// when ENABLE_PROCESSED_INPUT is disabled (raw console mode).
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

        // Thread for Ctrl-Break handling (quit signal)
        let quit_tx = tx;
        thread::Builder::new()
            .name("blocking_ctrl_handler".to_string())
            .spawn(move || {
                use windows_sys::Win32::System::Console::{
                    SetConsoleCtrlHandler, CTRL_BREAK_EVENT,
                };

                static QUIT_FLAG: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);

                unsafe extern "system" fn handler(ctrl_type: u32) -> i32 {
                    match ctrl_type {
                        CTRL_BREAK_EVENT => {
                            QUIT_FLAG.store(true, std::sync::atomic::Ordering::SeqCst);
                            1 // handled
                        },
                        // Prevent default termination for CTRL_C_EVENT but don't
                        // intercept it — with ENABLE_PROCESSED_INPUT disabled,
                        // byte 0x03 flows through ReadFile directly.
                        _ => 1,
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
