//! IPC stuff for starting to split things into a client and server model.
use crate::{
    data::{ClientId, ConnectToSession, KeyWithModifier, Style},
    errors::{prelude::*, ErrorContext},
    input::{actions::Action, cli_assets::CliAssets},
    pane_size::{Size, SizeInPixels},
};
use interprocess::local_socket::{prelude::*, Name, Stream as LocalSocketStream};
#[cfg(not(windows))]
use interprocess::local_socket::GenericFilePath;
use log::warn;
use serde::{Deserialize, Serialize};
use std::{
    fmt::{Display, Error, Formatter},
    io::{self, Read, Write},
    marker::PhantomData,
    path::Path,
};

// Protobuf imports
use crate::client_server_contract::client_server_contract::{
    ClientToServerMsg as ProtoClientToServerMsg, ServerToClientMsg as ProtoServerToClientMsg,
};
use prost::Message;

mod enum_conversions;
mod protobuf_conversion;

#[cfg(test)]
mod tests;

/// Convert a filesystem path to an IPC socket name.
///
/// On Unix, this passes through to `to_fs_name::<GenericFilePath>()` (Unix domain socket).
/// On Windows, named pipes require `\\.\pipe\name` format, so we derive a deterministic
/// pipe name from the last two path components (e.g. `contract_version_1/session_name`
/// becomes `\\.\pipe\zellij-contract_version_1-session_name`).
pub fn path_to_ipc_name(path: &Path) -> io::Result<Name<'_>> {
    #[cfg(not(windows))]
    {
        path.to_fs_name::<GenericFilePath>()
    }
    #[cfg(windows)]
    {
        path_to_windows_pipe_name(path, "")
    }
}

/// On Windows, returns a second named pipe name for the server→client direction.
///
/// Windows named pipes in synchronous mode deadlock when using DuplicateHandle for
/// concurrent read/write on the same pipe instance. To work around this, we use two
/// separate pipes: one for client→server (main) and one for server→client (reverse).
#[cfg(windows)]
pub fn path_to_ipc_name_reverse(path: &Path) -> io::Result<Name<'static>> {
    path_to_windows_pipe_name(path, "-srv")
}

// Security note: pipe names derived from path components are predictable, but this is
// mitigated by accept_secure_pipe_connection() which creates pipes with:
//   - ACL restricting access to the current user (SDDL `D:P(A;;GA;;;{SID})`)
//   - nMaxInstances = 1 (prevents pipe squatting — attacker can't create a second instance)
// Adding randomness would require a shared secret mechanism between client and server,
// adding complexity for marginal benefit given the above protections.
#[cfg(windows)]
fn path_to_windows_pipe_name(path: &Path, suffix: &str) -> io::Result<Name<'static>> {
    use interprocess::local_socket::GenericNamespaced;
    let components: Vec<&str> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    let name = if components.len() >= 2 {
        let len = components.len();
        format!(
            "zellij-{}-{}{}",
            components[len - 2],
            components[len - 1],
            suffix
        )
    } else {
        format!(
            "zellij-{}{}",
            path.display()
                .to_string()
                .replace(['\\', '/', ':'], "-"),
            suffix
        )
    };
    name.to_ns_name::<GenericNamespaced>()
}

type SessionId = u64;

/// A bidirectional byte stream that supports cloning for simultaneous read/write.
pub trait IpcStream: Read + Write + Send + 'static {
    fn try_clone_stream(&self) -> io::Result<Box<dyn IpcStream>>;
}

impl IpcStream for LocalSocketStream {
    fn try_clone_stream(&self) -> io::Result<Box<dyn IpcStream>> {
        use interprocess::TryClone;
        Ok(Box::new(self.try_clone()?))
    }
}

#[derive(PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct Session {
    // Unique ID for this session
    id: SessionId,
    // Identifier for the underlying IPC primitive (socket, pipe)
    conn_name: String,
    // User configured alias for the session
    alias: String,
}

// How do we want to connect to a session?
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientType {
    Reader,
    Writer,
}

#[derive(Default, Serialize, Deserialize, Debug, Clone)]
pub struct ClientAttributes {
    pub size: Size,
    pub style: Style,
}

#[derive(Default, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelDimensions {
    pub text_area_size: Option<SizeInPixels>,
    pub character_cell_size: Option<SizeInPixels>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct PaneReference {
    pub pane_id: u32,
    pub is_plugin: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct ColorRegister {
    pub index: usize,
    pub color: String,
}

impl PixelDimensions {
    pub fn merge(&mut self, other: PixelDimensions) {
        if let Some(text_area_size) = other.text_area_size {
            self.text_area_size = Some(text_area_size);
        }
        if let Some(character_cell_size) = other.character_cell_size {
            self.character_cell_size = Some(character_cell_size);
        }
    }
}

// Types of messages sent from the client to the server
#[allow(clippy::large_enum_variant)]
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ClientToServerMsg {
    DetachSession {
        client_ids: Vec<ClientId>,
    },
    TerminalPixelDimensions {
        pixel_dimensions: PixelDimensions,
    },
    BackgroundColor {
        color: String,
    },
    ForegroundColor {
        color: String,
    },
    ColorRegisters {
        color_registers: Vec<ColorRegister>,
    },
    TerminalResize {
        new_size: Size,
    },
    FirstClientConnected {
        cli_assets: CliAssets,
        is_web_client: bool,
    },
    AttachClient {
        cli_assets: CliAssets,
        tab_position_to_focus: Option<usize>,
        pane_to_focus: Option<PaneReference>,
        is_web_client: bool,
    },
    AttachWatcherClient {
        terminal_size: Size,
        is_web_client: bool,
    },
    Action {
        action: Action,
        terminal_id: Option<u32>,
        client_id: Option<ClientId>,
        is_cli_client: bool,
    },
    Key {
        key: KeyWithModifier,
        raw_bytes: Vec<u8>,
        is_kitty_keyboard_protocol: bool,
    },
    ClientExited,
    KillSession,
    ConnStatus,
    WebServerStarted {
        base_url: String,
    },
    FailedToStartWebServer {
        error: String,
    },
}

// Types of messages sent from the server to the client
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ServerToClientMsg {
    Render {
        content: String,
    },
    UnblockInputThread,
    Exit {
        exit_reason: ExitReason,
    },
    Connected,
    Log {
        lines: Vec<String>,
    },
    LogError {
        lines: Vec<String>,
    },
    SwitchSession {
        connect_to_session: ConnectToSession,
    },
    UnblockCliPipeInput {
        pipe_name: String,
    },
    CliPipeOutput {
        pipe_name: String,
        output: String,
    },
    QueryTerminalSize,
    StartWebServer,
    RenamedSession {
        name: String,
    },
    ConfigFileUpdated,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ExitReason {
    Normal,
    NormalDetached,
    ForceDetached,
    CannotAttach,
    Disconnect,
    WebClientsForbidden,
    KickedByHost,
    CustomExitStatus(i32),
    Error(String),
}

impl Display for ExitReason {
    fn fmt(&self, f: &mut Formatter) -> Result<(), Error> {
        match self {
            Self::Normal => write!(f, "Bye from Zellij!"),
            Self::NormalDetached => write!(f, "Session detached"),
            Self::ForceDetached => write!(
                f,
                "Session was detached from this client (possibly because another client connected)"
            ),
            Self::CannotAttach => write!(
                f,
                "Session attached to another client. Use --force flag to force connect."
            ),
            Self::WebClientsForbidden => write!(
                f,
                "Web clients are not allowed in this session - cannot attach"
            ),
            Self::Disconnect => {
                let session_tip = match crate::envs::get_session_name() {
                    Ok(name) => format!("`zellij attach {}`", name),
                    Err(_) => "see `zellij ls` and `zellij attach`".to_string(),
                };
                write!(
                    f,
                    "
Your zellij client lost connection to the zellij server.

As a safety measure, you have been disconnected from the current zellij session.
However, the session should still exist and none of your data should be lost.

This usually means that your terminal didn't process server messages quick
enough. Maybe your system is currently under high load, or your terminal
isn't performant enough.

There are a few things you can try now:
    - Reattach to your previous session and see if it works out better this
      time: {session_tip}
    - Try using a faster (maybe GPU-accelerated) terminal emulator
    "
                )
            },
            Self::KickedByHost => write!(f, "Disconnected by host"),
            Self::CustomExitStatus(exit_status) => write!(f, "Exit {}", exit_status),
            Self::Error(e) => write!(f, "Error occurred in server:\n{}", e),
        }
    }
}

/// Sends messages on a stream socket, along with an [`ErrorContext`].
pub struct IpcSenderWithContext<T: Serialize> {
    sender: io::BufWriter<Box<dyn IpcStream>>,
    _phantom: PhantomData<T>,
}

impl<T: Serialize> IpcSenderWithContext<T> {
    /// Returns a sender to the given [LocalSocketStream](interprocess::local_socket::LocalSocketStream).
    pub fn new(sender: LocalSocketStream) -> Self {
        Self {
            sender: io::BufWriter::new(Box::new(sender)),
            _phantom: PhantomData,
        }
    }

    fn from_boxed(sender: Box<dyn IpcStream>) -> Self {
        Self {
            sender: io::BufWriter::new(sender),
            _phantom: PhantomData,
        }
    }

    pub fn send_client_msg(&mut self, msg: ClientToServerMsg) -> Result<()> {
        let proto_msg: ProtoClientToServerMsg = msg.into();
        write_protobuf_message(&mut self.sender, &proto_msg)?;
        let _ = self.sender.flush();
        Ok(())
    }

    pub fn send_server_msg(&mut self, msg: ServerToClientMsg) -> Result<()> {
        let proto_msg: ProtoServerToClientMsg = msg.into();
        write_protobuf_message(&mut self.sender, &proto_msg)?;
        let _ = self.sender.flush();
        Ok(())
    }

    /// Returns an [`IpcReceiverWithContext`] with the same socket as this sender.
    pub fn get_receiver<F>(&self) -> IpcReceiverWithContext<F>
    where
        F: for<'de> Deserialize<'de> + Serialize,
    {
        let socket = self.sender.get_ref().try_clone_stream().unwrap();
        IpcReceiverWithContext::from_boxed(socket)
    }
}

/// Receives messages on a stream socket, along with an [`ErrorContext`].
pub struct IpcReceiverWithContext<T> {
    receiver: io::BufReader<Box<dyn IpcStream>>,
    _phantom: PhantomData<T>,
}

impl<T> IpcReceiverWithContext<T>
where
    T: for<'de> Deserialize<'de> + Serialize,
{
    /// Returns a receiver to the given [LocalSocketStream](interprocess::local_socket::LocalSocketStream).
    pub fn new(receiver: LocalSocketStream) -> Self {
        Self {
            receiver: io::BufReader::new(Box::new(receiver)),
            _phantom: PhantomData,
        }
    }

    fn from_boxed(receiver: Box<dyn IpcStream>) -> Self {
        Self {
            receiver: io::BufReader::new(receiver),
            _phantom: PhantomData,
        }
    }

    pub fn recv_client_msg(&mut self) -> Option<(ClientToServerMsg, ErrorContext)> {
        match read_protobuf_message::<ProtoClientToServerMsg>(&mut self.receiver) {
            Ok(proto_msg) => match proto_msg.try_into() {
                Ok(rust_msg) => Some((rust_msg, ErrorContext::default())),
                Err(e) => {
                    warn!("Error converting protobuf to ClientToServerMsg: {:?}", e);
                    None
                },
            },
            Err(_e) => None,
        }
    }

    pub fn recv_server_msg(&mut self) -> Option<(ServerToClientMsg, ErrorContext)> {
        match read_protobuf_message::<ProtoServerToClientMsg>(&mut self.receiver) {
            Ok(proto_msg) => match proto_msg.try_into() {
                Ok(rust_msg) => Some((rust_msg, ErrorContext::default())),
                Err(e) => {
                    warn!("Error converting protobuf to ServerToClientMsg: {:?}", e);
                    None
                },
            },
            Err(_e) => None,
        }
    }

    /// Returns an [`IpcSenderWithContext`] with the same socket as this receiver.
    pub fn get_sender<F: Serialize>(&self) -> IpcSenderWithContext<F> {
        let socket = self.receiver.get_ref().try_clone_stream().unwrap();
        IpcSenderWithContext::from_boxed(socket)
    }
}

// Maximum IPC message size (64 MiB). Rejects length-prefixed messages larger
// than this to prevent a malicious or corrupted peer from causing OOM.
pub const MAX_IPC_MSG_SIZE: usize = 64 * 1024 * 1024;

// Protobuf wire format utilities
fn read_protobuf_message<T: Message + Default>(reader: &mut impl Read) -> Result<T> {
    // Read length-prefixed protobuf message
    let mut len_bytes = [0u8; 4];
    reader.read_exact(&mut len_bytes)?;
    let len = u32::from_le_bytes(len_bytes) as usize;

    if len > MAX_IPC_MSG_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("IPC message too large: {} bytes", len),
        )
        .into());
    }

    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;

    T::decode(&buf[..]).map_err(Into::into)
}

fn write_protobuf_message<T: Message>(writer: &mut impl Write, msg: &T) -> Result<()> {
    let encoded = msg.encode_to_vec();
    let len = encoded.len() as u32;

    // we measure the length of the message and transmit it first so that the reader will be able
    // to first read exactly 4 bytes (representing this length) and then read that amount of bytes
    // as the actual message - this is so that we are able to distinct whole messages over the wire
    // stream
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&encoded)?;
    Ok(())
}

// Protobuf helper functions
pub fn send_protobuf_client_to_server(
    sender: &mut IpcSenderWithContext<ClientToServerMsg>,
    msg: ClientToServerMsg,
) -> Result<()> {
    let proto_msg: ProtoClientToServerMsg = msg.into();
    write_protobuf_message(&mut sender.sender, &proto_msg)?;
    let _ = sender.sender.flush();
    Ok(())
}

pub fn send_protobuf_server_to_client(
    sender: &mut IpcSenderWithContext<ServerToClientMsg>,
    msg: ServerToClientMsg,
) -> Result<()> {
    let proto_msg: ProtoServerToClientMsg = msg.into();
    write_protobuf_message(&mut sender.sender, &proto_msg)?;
    let _ = sender.sender.flush();
    Ok(())
}

pub fn recv_protobuf_client_to_server(
    receiver: &mut IpcReceiverWithContext<ClientToServerMsg>,
) -> Option<(ClientToServerMsg, ErrorContext)> {
    match read_protobuf_message::<ProtoClientToServerMsg>(&mut receiver.receiver) {
        Ok(proto_msg) => match proto_msg.try_into() {
            Ok(rust_msg) => Some((rust_msg, ErrorContext::default())),
            Err(e) => {
                warn!("Error converting protobuf message: {:?}", e);
                None
            },
        },
        Err(_e) => None,
    }
}

pub fn recv_protobuf_server_to_client(
    receiver: &mut IpcReceiverWithContext<ServerToClientMsg>,
) -> Option<(ServerToClientMsg, ErrorContext)> {
    match read_protobuf_message::<ProtoServerToClientMsg>(&mut receiver.receiver) {
        Ok(proto_msg) => match proto_msg.try_into() {
            Ok(rust_msg) => Some((rust_msg, ErrorContext::default())),
            Err(e) => {
                warn!("Error converting protobuf message: {:?}", e);
                None
            },
        },
        Err(_e) => None,
    }
}

/// Creates a named pipe with a security descriptor restricting access to the current user,
/// waits for a client connection, and returns the connected pipe as a `std::fs::File`.
///
/// The pipe is created with `nMaxInstances = 1` to prevent pipe squatting — if zellij creates
/// the pipe first, an attacker cannot create another instance with the same name.
/// The ACL uses SDDL `D:P(A;;GA;;;{SID})` granting only the current user Generic All access.
#[cfg(windows)]
pub fn accept_secure_pipe_connection(path: &Path) -> io::Result<std::fs::File> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{
        CloseHandle, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, PSECURITY_DESCRIPTOR, TokenUser, SECURITY_ATTRIBUTES,
        TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    };
    use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // Compute pipe name (same logic as path_to_windows_pipe_name)
    let components: Vec<&str> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    let name = if components.len() >= 2 {
        let len = components.len();
        format!("zellij-{}-{}", components[len - 2], components[len - 1])
    } else {
        format!(
            "zellij-{}",
            path.display()
                .to_string()
                .replace(['\\', '/', ':'], "-")
        )
    };
    let pipe_path = format!("\\\\.\\pipe\\{}", name);
    let pipe_path_wide: Vec<u16> = pipe_path.encode_utf16().chain(std::iter::once(0)).collect();

    unsafe {
        // 1. Get current user SID via process token
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut needed: u32 = 0;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);

        let mut token_buf = vec![0u8; needed as usize];
        if GetTokenInformation(
            token,
            TokenUser,
            token_buf.as_mut_ptr() as _,
            needed,
            &mut needed,
        ) == 0
        {
            let err = io::Error::last_os_error();
            CloseHandle(token);
            return Err(err);
        }
        CloseHandle(token);

        let token_user = &*(token_buf.as_ptr() as *const TOKEN_USER);
        let mut sid_wide: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(token_user.User.Sid, &mut sid_wide) == 0 {
            return Err(io::Error::last_os_error());
        }

        // Convert wide SID string to Rust String
        let sid_str = {
            let mut len = 0;
            while *sid_wide.add(len) != 0 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(sid_wide, len);
            String::from_utf16_lossy(slice)
        };
        LocalFree(sid_wide as _);

        // 2. Build SDDL: Protected DACL, only current user gets Generic All
        let sddl = format!("D:P(A;;GA;;;{})", sid_str);
        let sddl_wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();

        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl_wide.as_ptr(),
            1, // SDDL_REVISION_1
            &mut sd,
            std::ptr::null_mut(),
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }

        // 3. Create named pipe with security attributes and nMaxInstances=1
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };

        let handle = CreateNamedPipeW(
            pipe_path_wide.as_ptr(),
            PIPE_ACCESS_DUPLEX,
            0, // PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT (all zero)
            1,    // nMaxInstances — prevents pipe squatting
            4096, // output buffer size
            4096, // input buffer size
            0,    // default timeout
            &sa,
        );

        LocalFree(sd as _);

        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }

        // 4. Wait for a client to connect
        if ConnectNamedPipe(handle, std::ptr::null_mut()) == 0 {
            let err = io::Error::last_os_error();
            // ERROR_PIPE_CONNECTED (535) means client connected before ConnectNamedPipe
            if err.raw_os_error() != Some(535) {
                CloseHandle(handle);
                return Err(err);
            }
        }

        Ok(std::fs::File::from_raw_handle(handle as _))
    }
}
