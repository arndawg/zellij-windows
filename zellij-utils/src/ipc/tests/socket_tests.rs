use crate::ipc::{
    ClientToServerMsg, IpcReceiverWithContext, IpcSenderWithContext, ServerToClientMsg,
};
#[cfg(not(windows))]
use crate::pane_size::Size;
use interprocess::local_socket::{prelude::*, ListenerOptions, Stream as LocalSocketStream};
#[cfg(not(windows))]
use interprocess::local_socket::GenericFilePath;
#[cfg(not(windows))]
use std::os::unix::fs::FileTypeExt;
#[cfg(not(windows))]
use std::path::PathBuf;
use tempfile::TempDir;

#[cfg(not(windows))]
fn socket_path() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("failed to create temp dir");
    let path = dir.path().join("test.sock");
    (dir, path)
}

#[cfg(not(windows))]
#[test]
fn client_to_server_message_over_socket() {
    let (_dir, path) = socket_path();
    let listener = ListenerOptions::new().name(path.as_path().to_fs_name::<GenericFilePath>().unwrap()).create_sync().expect("bind failed");

    let client = std::thread::spawn({
        let path = path.clone();
        move || {
            let stream = LocalSocketStream::connect(path.as_path().to_fs_name::<GenericFilePath>().unwrap()).expect("connect failed");
            let mut sender: IpcSenderWithContext<ClientToServerMsg> =
                IpcSenderWithContext::new(stream);
            sender
                .send_client_msg(ClientToServerMsg::ConnStatus)
                .expect("send failed");
        }
    });

    let stream = listener.incoming().next().unwrap().expect("accept failed");
    let mut receiver: IpcReceiverWithContext<ClientToServerMsg> =
        IpcReceiverWithContext::new(stream);

    let msg = receiver.recv_client_msg();
    assert!(msg.is_some(), "should receive a message");
    let (msg, _ctx) = msg.unwrap();
    assert!(
        matches!(msg, ClientToServerMsg::ConnStatus),
        "should be ConnStatus, got: {:?}",
        msg
    );

    client.join().expect("client thread panicked");
}

#[cfg(not(windows))]
#[test]
fn server_to_client_message_over_socket() {
    let (_dir, path) = socket_path();
    let listener = ListenerOptions::new().name(path.as_path().to_fs_name::<GenericFilePath>().unwrap()).create_sync().expect("bind failed");

    let server = std::thread::spawn(move || {
        let stream = listener.incoming().next().unwrap().expect("accept failed");
        let mut sender: IpcSenderWithContext<ServerToClientMsg> =
            IpcSenderWithContext::new(stream);
        sender
            .send_server_msg(ServerToClientMsg::Connected)
            .expect("send failed");
    });

    let stream = LocalSocketStream::connect(path.as_path().to_fs_name::<GenericFilePath>().unwrap()).expect("connect failed");
    let mut receiver: IpcReceiverWithContext<ServerToClientMsg> =
        IpcReceiverWithContext::new(stream);

    let msg = receiver.recv_server_msg();
    assert!(msg.is_some(), "should receive a message");
    let (msg, _ctx) = msg.unwrap();
    assert!(
        matches!(msg, ServerToClientMsg::Connected),
        "should be Connected, got: {:?}",
        msg
    );

    server.join().expect("server thread panicked");
}

#[cfg(not(windows))]
#[test]
fn bidirectional_communication_via_fd_duplication() {
    let (_dir, path) = socket_path();
    let listener = ListenerOptions::new().name(path.as_path().to_fs_name::<GenericFilePath>().unwrap()).create_sync().expect("bind failed");

    let server = std::thread::spawn(move || {
        let stream = listener.incoming().next().unwrap().expect("accept failed");
        let mut sender: IpcSenderWithContext<ServerToClientMsg> =
            IpcSenderWithContext::new(stream);

        // Create a receiver from the same socket via dup()
        let mut receiver: IpcReceiverWithContext<ClientToServerMsg> = sender.get_receiver();

        sender
            .send_server_msg(ServerToClientMsg::Connected)
            .expect("send failed");

        let msg = receiver.recv_client_msg();
        assert!(msg.is_some(), "server should receive client message");
        let (msg, _) = msg.unwrap();
        assert!(
            matches!(msg, ClientToServerMsg::ConnStatus),
            "should be ConnStatus"
        );
    });

    let stream = LocalSocketStream::connect(path.as_path().to_fs_name::<GenericFilePath>().unwrap()).expect("connect failed");
    let mut sender: IpcSenderWithContext<ClientToServerMsg> = IpcSenderWithContext::new(stream);

    // Create a receiver from the same socket via dup()
    let mut receiver: IpcReceiverWithContext<ServerToClientMsg> = sender.get_receiver();

    let msg = receiver.recv_server_msg();
    assert!(msg.is_some(), "client should receive server message");
    let (msg, _) = msg.unwrap();
    assert!(
        matches!(msg, ServerToClientMsg::Connected),
        "should be Connected"
    );

    sender
        .send_client_msg(ClientToServerMsg::ConnStatus)
        .expect("send failed");

    server.join().expect("server thread panicked");
}

#[cfg(not(windows))]
#[test]
fn multiple_messages_in_sequence() {
    let (_dir, path) = socket_path();
    let listener = ListenerOptions::new().name(path.as_path().to_fs_name::<GenericFilePath>().unwrap()).create_sync().expect("bind failed");

    let client = std::thread::spawn({
        let path = path.clone();
        move || {
            let stream = LocalSocketStream::connect(path.as_path().to_fs_name::<GenericFilePath>().unwrap()).expect("connect failed");
            let mut sender: IpcSenderWithContext<ClientToServerMsg> =
                IpcSenderWithContext::new(stream);

            sender
                .send_client_msg(ClientToServerMsg::ConnStatus)
                .expect("send 1 failed");
            sender
                .send_client_msg(ClientToServerMsg::TerminalResize {
                    new_size: Size { rows: 50, cols: 120 },
                })
                .expect("send 2 failed");
            sender
                .send_client_msg(ClientToServerMsg::KillSession)
                .expect("send 3 failed");
        }
    });

    let stream = listener.incoming().next().unwrap().expect("accept failed");
    let mut receiver: IpcReceiverWithContext<ClientToServerMsg> =
        IpcReceiverWithContext::new(stream);

    let (msg1, _) = receiver.recv_client_msg().expect("missing message 1");
    assert!(matches!(msg1, ClientToServerMsg::ConnStatus));

    let (msg2, _) = receiver.recv_client_msg().expect("missing message 2");
    match msg2 {
        ClientToServerMsg::TerminalResize { new_size } => {
            assert_eq!(new_size.rows, 50);
            assert_eq!(new_size.cols, 120);
        },
        other => panic!("expected TerminalResize, got: {:?}", other),
    }

    let (msg3, _) = receiver.recv_client_msg().expect("missing message 3");
    assert!(matches!(msg3, ClientToServerMsg::KillSession));

    client.join().expect("client thread panicked");
}

#[cfg(not(windows))]
#[test]
fn receiver_returns_none_on_closed_connection() {
    let (_dir, path) = socket_path();
    let listener = ListenerOptions::new().name(path.as_path().to_fs_name::<GenericFilePath>().unwrap()).create_sync().expect("bind failed");

    let client = std::thread::spawn({
        let path = path.clone();
        move || {
            let stream = LocalSocketStream::connect(path.as_path().to_fs_name::<GenericFilePath>().unwrap()).expect("connect failed");
            let mut sender: IpcSenderWithContext<ClientToServerMsg> =
                IpcSenderWithContext::new(stream);
            sender
                .send_client_msg(ClientToServerMsg::ConnStatus)
                .expect("send failed");
            // sender drops here, closing the connection
        }
    });

    let stream = listener.incoming().next().unwrap().expect("accept failed");
    let mut receiver: IpcReceiverWithContext<ClientToServerMsg> =
        IpcReceiverWithContext::new(stream);

    client.join().expect("client thread panicked");

    let msg = receiver.recv_client_msg();
    assert!(msg.is_some(), "should receive the sent message");

    // After the sender is dropped, subsequent reads should return None
    let msg = receiver.recv_client_msg();
    assert!(msg.is_none(), "should return None after connection closed");
}

// --- Session discovery tests ---
// These test the OS-specific mechanics used by get_sessions() in sessions.rs:
// FileTypeExt::is_socket() for identifying socket files, and the assert_socket
// probing pattern (connect, send ConnStatus, expect Connected).

#[cfg(not(windows))]
#[test]
fn is_socket_identifies_bound_unix_socket() {
    let (_dir, path) = socket_path();
    let _listener = ListenerOptions::new().name(path.as_path().to_fs_name::<GenericFilePath>().unwrap()).create_sync().expect("bind failed");

    let metadata = std::fs::metadata(&path).expect("metadata failed");
    assert!(
        metadata.file_type().is_socket(),
        "a bound LocalSocketListener path should be identified as a socket"
    );
}

#[cfg(not(windows))]
#[test]
fn is_socket_rejects_regular_file() {
    let dir = TempDir::new().expect("failed to create temp dir");
    let file_path = dir.path().join("not_a_socket");
    std::fs::write(&file_path, b"regular file").expect("write failed");

    let metadata = std::fs::metadata(&file_path).expect("metadata failed");
    assert!(
        !metadata.file_type().is_socket(),
        "a regular file should NOT be identified as a socket"
    );
}

#[cfg(not(windows))]
#[test]
fn session_probe_accepts_responding_socket() {
    // Simulates the assert_socket() pattern from sessions.rs:
    // A real Zellij server responds to ConnStatus with Connected.
    let (_dir, path) = socket_path();
    let listener = ListenerOptions::new().name(path.as_path().to_fs_name::<GenericFilePath>().unwrap()).create_sync().expect("bind failed");

    // Spawn a fake "server" that responds to ConnStatus with Connected
    let server = std::thread::spawn(move || {
        let stream = listener.incoming().next().unwrap().expect("accept failed");
        let mut receiver: IpcReceiverWithContext<ClientToServerMsg> =
            IpcReceiverWithContext::new(stream);
        let mut sender: IpcSenderWithContext<ServerToClientMsg> = receiver.get_sender();

        let msg = receiver.recv_client_msg();
        assert!(matches!(
            msg,
            Some((ClientToServerMsg::ConnStatus, _))
        ));

        sender
            .send_server_msg(ServerToClientMsg::Connected)
            .expect("send failed");
    });

    // Client-side probing (mirrors assert_socket in sessions.rs)
    let stream = LocalSocketStream::connect(path.as_path().to_fs_name::<GenericFilePath>().unwrap()).expect("connect failed");
    let mut sender: IpcSenderWithContext<ClientToServerMsg> = IpcSenderWithContext::new(stream);
    sender
        .send_client_msg(ClientToServerMsg::ConnStatus)
        .expect("send failed");
    let mut receiver: IpcReceiverWithContext<ServerToClientMsg> = sender.get_receiver();

    let result = receiver.recv_server_msg();
    assert!(
        matches!(result, Some((ServerToClientMsg::Connected, _))),
        "probing a live session socket should return Connected"
    );

    server.join().expect("server thread panicked");
}

#[cfg(not(windows))]
#[test]
fn session_probe_rejects_dead_socket() {
    // Simulates discovering a stale socket file with no listener.
    // get_sessions() filters these out via assert_socket() which tries to connect.
    let (_dir, path) = socket_path();

    // Bind and immediately drop the listener to create a stale socket file
    {
        let _listener = ListenerOptions::new().name(path.as_path().to_fs_name::<GenericFilePath>().unwrap()).create_sync().expect("bind failed");
    }
    // Listener is dropped — the socket file may still exist but nobody is listening

    let result = LocalSocketStream::connect(path.as_path().to_fs_name::<GenericFilePath>().unwrap());
    assert!(
        result.is_err(),
        "connecting to a dead socket should fail (no listener)"
    );
}

#[cfg(not(windows))]
#[test]
fn socket_directory_enumeration_finds_sockets() {
    // Simulates the readdir + is_socket() filtering pattern from get_sessions().
    let dir = TempDir::new().expect("failed to create temp dir");

    // Create a socket
    let sock_path = dir.path().join("test-session");
    let _listener = ListenerOptions::new().name(sock_path.as_path().to_fs_name::<GenericFilePath>().unwrap()).create_sync().expect("bind failed");

    // Create a regular file (should be filtered out)
    let file_path = dir.path().join("not-a-session");
    std::fs::write(&file_path, b"data").expect("write failed");

    // Enumerate the directory, filtering for sockets (same pattern as get_sessions)
    let entries: Vec<String> = std::fs::read_dir(dir.path())
        .expect("read_dir failed")
        .filter_map(|entry| {
            let entry = entry.ok()?;
            if entry.file_type().ok()?.is_socket() {
                entry.file_name().into_string().ok()
            } else {
                None
            }
        })
        .collect();

    assert_eq!(entries.len(), 1, "should find exactly one socket");
    assert_eq!(entries[0], "test-session");
}

/// On Windows, session probing uses dual named pipes: the client sends ConnStatus
/// on the main pipe and reads the Connected response from the reverse pipe.
/// This test simulates the full dual-pipe handshake used by assert_socket().
#[cfg(windows)]
#[test]
fn windows_dual_pipe_session_probe() {
    use crate::ipc::path_to_ipc_name;
    use crate::ipc::path_to_ipc_name_reverse;

    // Create a fake session path
    let dir = TempDir::new().expect("failed to create temp dir");
    // path_to_ipc_name needs at least 2 components for the pipe name
    let session_path = dir.path().join("contract_version_1").join("test_session");
    std::fs::create_dir_all(&session_path).ok();

    let main_name = path_to_ipc_name(&session_path).expect("main pipe name");
    let reverse_name = path_to_ipc_name_reverse(&session_path).expect("reverse pipe name");

    let main_listener = ListenerOptions::new()
        .name(main_name)
        .create_sync()
        .expect("main listener");
    let reverse_listener = ListenerOptions::new()
        .name(reverse_name.clone())
        .create_sync()
        .expect("reverse listener");

    // Spawn a fake server that mimics the real dual-pipe server
    let server = std::thread::spawn(move || {
        // Accept main connection (client→server)
        let main_stream = main_listener
            .incoming()
            .next()
            .unwrap()
            .expect("main accept");
        // Accept reverse connection (server→client)
        let reverse_stream = reverse_listener.accept().expect("reverse accept");

        // Read ConnStatus from main pipe
        let mut receiver: IpcReceiverWithContext<ClientToServerMsg> =
            IpcReceiverWithContext::new(main_stream);
        let msg = receiver.recv_client_msg();
        assert!(
            matches!(msg, Some((ClientToServerMsg::ConnStatus, _))),
            "server should receive ConnStatus"
        );

        // Send Connected on reverse pipe
        let mut sender: IpcSenderWithContext<ServerToClientMsg> =
            IpcSenderWithContext::new(reverse_stream);
        sender
            .send_server_msg(ServerToClientMsg::Connected)
            .expect("send Connected");
    });

    // Client-side probing (mirrors assert_socket_inner on Windows)
    let main_name = path_to_ipc_name(&session_path).expect("main pipe name");
    let main_stream =
        LocalSocketStream::connect(main_name).expect("connect main");
    let reverse_stream =
        LocalSocketStream::connect(reverse_name).expect("connect reverse");

    let mut sender: IpcSenderWithContext<ClientToServerMsg> =
        IpcSenderWithContext::new(main_stream);
    sender
        .send_client_msg(ClientToServerMsg::ConnStatus)
        .expect("send ConnStatus");

    let mut receiver: IpcReceiverWithContext<ServerToClientMsg> =
        IpcReceiverWithContext::new(reverse_stream);
    let result = receiver.recv_server_msg();
    assert!(
        matches!(result, Some((ServerToClientMsg::Connected, _))),
        "dual-pipe probe should return Connected, got: {:?}",
        result
    );

    server.join().expect("server thread panicked");
}

/// Tests that when a probe connects only to the main pipe (without the reverse
/// pipe), the server's listener doesn't permanently wedge. The server should
/// time out and continue accepting new connections.
#[cfg(windows)]
#[test]
fn windows_probe_without_reverse_pipe_does_not_wedge_server() {
    use crate::ipc::path_to_ipc_name;
    use crate::ipc::path_to_ipc_name_reverse;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    let dir = TempDir::new().expect("failed to create temp dir");
    let session_path = dir.path().join("contract_version_1").join("probe_test");
    std::fs::create_dir_all(&session_path).ok();

    let main_name = path_to_ipc_name(&session_path).expect("main pipe name");
    let reverse_name = path_to_ipc_name_reverse(&session_path).expect("reverse pipe name");

    let main_listener = ListenerOptions::new()
        .name(main_name)
        .create_sync()
        .expect("main listener");
    let reverse_listener = ListenerOptions::new()
        .name(reverse_name.clone())
        .create_sync()
        .expect("reverse listener");

    let second_client_served = Arc::new(AtomicBool::new(false));
    let second_client_served_clone = second_client_served.clone();

    // Spawn a server that mimics the real listener pattern:
    // a dedicated thread accepts reverse connections and feeds them via channel.
    let server = std::thread::spawn(move || {
        let (reverse_tx, reverse_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in reverse_listener.incoming() {
                match stream {
                    Ok(s) => {
                        if reverse_tx.send(s).is_err() {
                            break;
                        }
                    },
                    Err(_) => break,
                }
            }
        });

        for stream in main_listener.incoming() {
            match stream {
                Ok(main_stream) => {
                    match reverse_rx.recv_timeout(std::time::Duration::from_secs(2)) {
                        Ok(reverse_stream) => {
                            // Full client — read ConnStatus, send Connected
                            let mut receiver: IpcReceiverWithContext<ClientToServerMsg> =
                                IpcReceiverWithContext::new(main_stream);
                            let _msg = receiver.recv_client_msg();
                            let mut sender: IpcSenderWithContext<ServerToClientMsg> =
                                IpcSenderWithContext::new(reverse_stream);
                            let _ = sender.send_server_msg(ServerToClientMsg::Connected);
                            second_client_served_clone.store(true, Ordering::SeqCst);
                            break;
                        },
                        Err(_) => {
                            // Probe-only — no reverse connection, skip
                            drop(main_stream);
                            continue;
                        },
                    }
                },
                Err(_) => break,
            }
        }
    });

    // First: connect only to main pipe (simulate a bad probe)
    let main_name = path_to_ipc_name(&session_path).expect("main pipe name");
    let _probe_stream = LocalSocketStream::connect(main_name).expect("probe connect");
    // Don't connect to reverse pipe — drop the probe after a moment
    std::thread::sleep(std::time::Duration::from_secs(3));
    drop(_probe_stream);

    // Second: connect properly to both pipes (simulate a real client)
    let main_name = path_to_ipc_name(&session_path).expect("main pipe name");
    let main_stream = LocalSocketStream::connect(main_name).expect("real connect main");
    let reverse_stream =
        LocalSocketStream::connect(reverse_name).expect("real connect reverse");

    let mut sender: IpcSenderWithContext<ClientToServerMsg> =
        IpcSenderWithContext::new(main_stream);
    sender
        .send_client_msg(ClientToServerMsg::ConnStatus)
        .expect("send ConnStatus");

    let mut receiver: IpcReceiverWithContext<ServerToClientMsg> =
        IpcReceiverWithContext::new(reverse_stream);
    let result = receiver.recv_server_msg();
    assert!(
        matches!(result, Some((ServerToClientMsg::Connected, _))),
        "second (real) client should get Connected after probe timed out"
    );

    server.join().expect("server thread panicked");
    assert!(
        second_client_served.load(Ordering::SeqCst),
        "server should have served the second client"
    );
}
