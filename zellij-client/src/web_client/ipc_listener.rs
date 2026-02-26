use axum_server::Handle;
use std::io::{Read, Write};
use std::net::IpAddr;
#[cfg(unix)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
use zellij_utils::consts::WEBSERVER_SOCKET_PATH;
use zellij_utils::prost::Message;
use zellij_utils::web_server_commands::{InstructionForWebServer, VersionInfo, WebServerResponse};
use zellij_utils::web_server_contract::web_server_contract::InstructionForWebServer as ProtoInstructionForWebServer;
use zellij_utils::web_server_contract::web_server_contract::WebServerResponse as ProtoWebServerResponse;

#[cfg(unix)]
pub async fn create_webserver_receiver(
    id: &str,
) -> Result<UnixStream, Box<dyn std::error::Error + Send + Sync>> {
    std::fs::create_dir_all(&WEBSERVER_SOCKET_PATH.as_path())?;
    let socket_path = WEBSERVER_SOCKET_PATH.join(format!("{}", id));

    if socket_path.exists() {
        tokio::fs::remove_file(&socket_path).await?;
    }

    let listener = UnixListener::bind(&socket_path)?;
    let (stream, _) = listener.accept().await?;
    Ok(stream)
}

#[cfg(unix)]
pub async fn receive_webserver_instruction(
    receiver: &mut UnixStream,
) -> std::io::Result<InstructionForWebServer> {
    use zellij_utils::ipc::MAX_IPC_MSG_SIZE;

    // Read length prefix (4 bytes)
    let mut len_bytes = [0u8; 4];
    receiver.read_exact(&mut len_bytes).await?;
    let len = u32::from_le_bytes(len_bytes) as usize;

    if len > MAX_IPC_MSG_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("IPC message too large: {} bytes", len),
        ));
    }

    // Read protobuf message
    let mut buffer = vec![0u8; len];
    receiver.read_exact(&mut buffer).await?;

    // Decode protobuf message
    let proto_instruction = ProtoInstructionForWebServer::decode(&buffer[..])
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    // Convert to Rust type
    proto_instruction
        .try_into()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
}

#[cfg(unix)]
pub async fn send_webserver_response(
    sender: &mut UnixStream,
    response: WebServerResponse,
) -> std::io::Result<()> {
    let proto_response: ProtoWebServerResponse = response.into();
    let encoded = proto_response.encode_to_vec();
    let len = encoded.len() as u32;

    sender.write_all(&len.to_le_bytes()).await?;
    sender.write_all(&encoded).await?;
    sender.flush().await?;

    Ok(())
}

#[cfg(unix)]
pub async fn listen_to_web_server_instructions(
    server_handle: Handle,
    id: &str,
    web_server_ip: IpAddr,
    web_server_port: u16,
) {
    loop {
        let receiver = create_webserver_receiver(id).await;
        match receiver {
            Ok(mut receiver) => match receive_webserver_instruction(&mut receiver).await {
                Ok(instruction) => match instruction {
                    InstructionForWebServer::ShutdownWebServer => {
                        server_handle.shutdown();
                        break;
                    },
                    InstructionForWebServer::QueryVersion => {
                        let response = WebServerResponse::Version(VersionInfo {
                            version: zellij_utils::consts::VERSION.to_string(),
                            ip: web_server_ip.to_string(),
                            port: web_server_port,
                        });
                        let _ = send_webserver_response(&mut receiver, response).await;
                    },
                },
                Err(e) => {
                    log::error!("Failed to process web server instruction: {}", e);
                },
            },
            Err(e) => {
                log::error!("Failed to listen to ipc channel: {}", e);
                break;
            },
        }
    }
}

// Windows implementation using ACL-secured named pipes (via CreateNamedPipeW).
// Wrapped in spawn_blocking since named pipe I/O is synchronous.
#[cfg(windows)]
pub async fn listen_to_web_server_instructions(
    server_handle: Handle,
    id: &str,
    web_server_ip: IpAddr,
    web_server_port: u16,
) {
    std::fs::create_dir_all(&WEBSERVER_SOCKET_PATH.as_path()).ok();
    let socket_path = WEBSERVER_SOCKET_PATH.join(format!("{}", id));

    // Create marker file so discover_webserver_sockets() can find this instance.
    // Named pipes are kernel objects, not filesystem entries, so we need this
    // marker for the same discovery mechanism Unix uses with socket files.
    if let Err(e) = std::fs::File::create(&socket_path) {
        log::error!("Failed to create web server marker file: {}", e);
        return;
    }

    loop {
        let path = socket_path.clone();
        let ip = web_server_ip;
        let port = web_server_port;
        let handle = server_handle.clone();

        let result = tokio::task::spawn_blocking(move || -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
            let mut stream = zellij_utils::ipc::accept_secure_pipe_connection(&path)?;

            // Read length prefix (4 bytes)
            let mut len_bytes = [0u8; 4];
            stream.read_exact(&mut len_bytes)?;
            let len = u32::from_le_bytes(len_bytes) as usize;

            if len > zellij_utils::ipc::MAX_IPC_MSG_SIZE {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("IPC message too large: {} bytes", len),
                )));
            }

            // Read protobuf message
            let mut buffer = vec![0u8; len];
            stream.read_exact(&mut buffer)?;

            let proto_instruction = ProtoInstructionForWebServer::decode(&buffer[..])
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
            let instruction: InstructionForWebServer = proto_instruction
                .try_into()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

            match instruction {
                InstructionForWebServer::ShutdownWebServer => {
                    handle.shutdown();
                    Ok(true) // signal to break
                },
                InstructionForWebServer::QueryVersion => {
                    let response = WebServerResponse::Version(VersionInfo {
                        version: zellij_utils::consts::VERSION.to_string(),
                        ip: ip.to_string(),
                        port,
                    });
                    let proto_response: ProtoWebServerResponse = response.into();
                    let encoded = proto_response.encode_to_vec();
                    let len = encoded.len() as u32;
                    stream.write_all(&len.to_le_bytes())?;
                    stream.write_all(&encoded)?;
                    stream.flush()?;
                    Ok(false)
                },
            }
        })
        .await;

        match result {
            Ok(Ok(true)) => break,  // shutdown requested
            Ok(Ok(false)) => {},     // handled, continue listening
            Ok(Err(e)) => {
                log::error!("Failed to process web server instruction: {}", e);
                break;
            },
            Err(e) => {
                log::error!("Failed to listen to ipc channel: {}", e);
                break;
            },
        }
    }

    // Clean up marker file
    let _ = std::fs::remove_file(&socket_path);
}
