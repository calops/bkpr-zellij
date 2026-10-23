//! The `[cli_client]` is used to attach to a running server session
//! and dispatch actions, that are specified through the command line.
use std::collections::{BTreeMap, HashSet};
use std::io::{self, BufRead, Read, Write};
use std::process;
use std::str::FromStr;
use std::time::{Duration, Instant};
use std::{fs, path::PathBuf};

use interprocess::{local_socket::prelude::*, TryClone};
use zellij_utils::control::{
    ControlError, ControlRequest, ControlResponse, MAX_REQUEST_BYTES, PROTOCOL,
};

use crate::os_input_output::ClientOsApi;
use uuid::Uuid;
use zellij_utils::{
    cli::{SubscribeCli, SubscribeFormat},
    data::PaneId,
    errors::prelude::*,
    input::actions::Action,
    ipc::{ClientToServerMsg, ExitReason, ServerToClientMsg},
};

const BKPR_CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

/// Read one JSON request and write exactly one JSON response. This path never discovers,
/// attaches to, or retries a session.
pub fn start_bkpr_control_client(session_name: Option<&str>) -> i32 {
    let response = match read_bkpr_control_request(io::stdin().lock()) {
        Ok(request) => match session_name {
            Some(session_name) if !session_name.is_empty() => {
                exchange_bkpr_control_request(session_name, &request)
            },
            _ => ControlResponse::failed(
                &request,
                "explicit_session_required",
                "bkpr-control requires an explicit --session NAME",
            ),
        },
        Err(error) => ControlResponse {
            protocol: PROTOCOL,
            request_id: String::new(),
            outcome: "failed".to_owned(),
            result: None,
            error: Some(ControlError {
                code: "invalid_request".to_owned(),
                message: error.to_string(),
            }),
        },
    };
    let mut stdout = io::stdout().lock();
    if let Err(error) = serde_json::to_writer(&mut stdout, &response)
        .map_err(io::Error::other)
        .and_then(|()| stdout.write_all(b"\n"))
        .and_then(|()| stdout.flush())
    {
        eprintln!("Could not write bkpr-control response: {error}");
        return 2;
    }
    match response.outcome.as_str() {
        "completed" | "dispatched" => 0,
        _ => 2,
    }
}

fn read_bkpr_control_request(reader: impl Read) -> io::Result<ControlRequest> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_REQUEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control request exceeds the 1 MiB limit",
        ));
    }
    // from_slice requires EOF after the value (apart from JSON whitespace).
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn exchange_bkpr_control_request(session_name: &str, request: &ControlRequest) -> ControlResponse {
    use zellij_utils::ipc::{IpcReceiverWithContext, IpcSenderWithContext};

    // ClientOsApi has neither bounded connection attempts nor socket timeouts.
    // Keep the native IPC codec, but own this connection so its reads really time out.
    let socket_path = zellij_utils::consts::ZELLIJ_SOCK_DIR.join(session_name);
    let socket = match zellij_utils::consts::ipc_connect(&socket_path) {
        Ok(socket) => socket,
        Err(error) => {
            return ControlResponse::failed(request, "connection_failed", error.to_string());
        },
    };
    let channels = (|| -> io::Result<_> {
        socket.set_recv_timeout(Some(BKPR_CONTROL_TIMEOUT))?;
        socket.set_send_timeout(Some(BKPR_CONTROL_TIMEOUT))?;
        Ok((
            IpcSenderWithContext::<ClientToServerMsg>::new(socket.try_clone()?),
            IpcReceiverWithContext::<ServerToClientMsg>::new(socket.try_clone()?),
        ))
    })();
    let (mut sender, mut receiver) = match channels {
        Ok(channels) => channels,
        Err(error) => {
            return ControlResponse::failed(request, "connection_failed", error.to_string());
        },
    };
    // LEGACY_COMPAT(next-major): an old server may close on the unknown IPC variant.
    // Probe using a read-only method before ever transmitting the caller's mutation.
    let probe = ControlRequest {
        protocol: PROTOCOL,
        request_id: Uuid::new_v4().to_string(),
        session_id: request.session_id.clone(),
        source_pane_id: request.source_pane_id.clone(),
        client_id: request.client_id,
        method: "capabilities".to_owned(),
        arguments: serde_json::json!({}),
    };
    if let Err(error) = sender.send_client_msg(ClientToServerMsg::BkprControl {
        request: probe.clone(),
    }) {
        return ControlResponse::failed(
            request,
            "unsupported",
            format!("Native control capability probe could not be sent: {error}; request not sent"),
        );
    }
    match receive_bkpr_control_response(&socket, &mut receiver, &probe) {
        Ok(response) if response.outcome == "completed" => {},
        Ok(response) => {
            let detail = response
                .error
                .map(|error| error.message)
                .unwrap_or_else(|| response.outcome);
            return ControlResponse::failed(
                request,
                "unsupported",
                format!("Native control capability probe failed: {detail}; request not sent"),
            );
        },
        Err(error) => {
            return ControlResponse::failed(
                request,
                "unsupported",
                format!("Server did not confirm native control support: {error}; request not sent"),
            );
        },
    }
    if let Err(error) = sender.send_client_msg(ClientToServerMsg::BkprControl {
        request: request.clone(),
    }) {
        return ControlResponse::unknown(
            request,
            format!("Control request send failed: {error}; completion unknown; do not retry"),
        );
    }
    match receive_bkpr_control_response(&socket, &mut receiver, request) {
        Ok(response) => response,
        Err(error) => ControlResponse::unknown(
            request,
            format!("Control response unavailable: {error}; completion unknown; do not retry"),
        ),
    }
}

fn receive_bkpr_control_response(
    socket: &interprocess::local_socket::Stream,
    receiver: &mut zellij_utils::ipc::IpcReceiverWithContext<ServerToClientMsg>,
    request: &ControlRequest,
) -> Result<ControlResponse, String> {
    let deadline = Instant::now() + BKPR_CONTROL_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("five-second response deadline expired".to_owned());
        }
        socket
            .set_recv_timeout(Some(remaining))
            .map_err(|error| error.to_string())?;
        match receiver.try_recv_server_msg().map_err(|error| {
            format!("{error} (connection closed, timed out, or response unreadable)")
        })? {
            (ServerToClientMsg::BkprControl { response }, _) => {
                if response.protocol != PROTOCOL || response.request_id != request.request_id {
                    return Err("mismatched control response protocol or request_id".to_owned());
                }
                return Ok(response);
            },
            (ServerToClientMsg::UnblockInputThread, _) => {},
            _ => return Err("server returned a non-control response".to_owned()),
        }
    }
}

#[cfg(test)]
mod bkpr_control_tests {
    use super::*;
    use std::io::Cursor;

    fn valid_request() -> Vec<u8> {
        br#"{"protocol":1,"request_id":"request-1","session_id":"session-1","method":"snapshot","arguments":{}}"#.to_vec()
    }

    #[test]
    fn reads_one_request_with_optional_context() {
        let request = read_bkpr_control_request(valid_request().as_slice()).unwrap();
        assert_eq!(request.request_id, "request-1");
        assert_eq!(request.source_pane_id, None);
        assert_eq!(request.client_id, None);
        assert_eq!(request.method, "snapshot");
    }

    #[test]
    fn accepts_exact_limit_including_trailing_whitespace() {
        let mut bytes = valid_request();
        bytes.resize(MAX_REQUEST_BYTES, b' ');
        assert_eq!(
            read_bkpr_control_request(bytes.as_slice())
                .unwrap()
                .protocol,
            PROTOCOL
        );
    }

    #[test]
    fn rejects_oversize_without_reading_unbounded_input() {
        let mut bytes = valid_request();
        bytes.resize(MAX_REQUEST_BYTES + 100, b' ');
        let mut reader = Cursor::new(bytes);
        let error = read_bkpr_control_request(&mut reader).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("1 MiB"));
        assert_eq!(reader.position(), MAX_REQUEST_BYTES as u64 + 1);
    }

    #[test]
    fn rejects_second_json_value_and_trailing_garbage() {
        for suffix in [b" {}".as_slice(), b"\nnull", b"x"] {
            let mut bytes = valid_request();
            bytes.extend_from_slice(suffix);
            assert!(read_bkpr_control_request(bytes.as_slice()).is_err());
        }
    }

    #[test]
    fn rejects_empty_truncated_and_invalid_utf8_requests() {
        for bytes in [b"".as_slice(), b"{", b"\xff"] {
            assert!(read_bkpr_control_request(bytes).is_err());
        }
    }

    #[test]
    fn rejects_unknown_request_fields() {
        let mut value: serde_json::Value = serde_json::from_slice(&valid_request()).unwrap();
        value["unexpected"] = serde_json::json!(true);
        assert!(read_bkpr_control_request(serde_json::to_vec(&value).unwrap().as_slice()).is_err());
    }

    #[test]
    fn preserves_input_read_failures() {
        struct BrokenInput;
        impl Read for BrokenInput {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "fixture input error",
                ))
            }
        }
        let error = read_bkpr_control_request(BrokenInput).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }
}

pub fn start_cli_client(
    mut os_input: Box<dyn ClientOsApi>,
    session_name: &str,
    actions: Vec<Action>,
) -> i32 {
    let zellij_ipc_pipe: PathBuf = {
        let mut sock_dir = zellij_utils::consts::ZELLIJ_SOCK_DIR.clone();
        fs::create_dir_all(&sock_dir).unwrap();
        zellij_utils::shared::set_permissions(&sock_dir, 0o700).unwrap();
        sock_dir.push(session_name);
        sock_dir
    };
    crate::check_ipc_pipe_length(&zellij_ipc_pipe);
    os_input.connect_to_server(&*zellij_ipc_pipe);
    let pane_id = os_input
        .env_variable("ZELLIJ_PANE_ID")
        .and_then(|e| e.trim().parse().ok());

    for action in actions {
        match action {
            Action::CliPipe {
                pipe_id,
                name,
                payload,
                plugin,
                args,
                configuration,
                launch_new,
                skip_cache,
                floating,
                in_place,
                cwd,
                pane_title,
            } => {
                pipe_client(
                    &mut os_input,
                    pipe_id,
                    name,
                    payload,
                    plugin,
                    args,
                    configuration,
                    launch_new,
                    skip_cache,
                    floating,
                    in_place,
                    pane_id,
                    cwd,
                    pane_title,
                );
            },
            action => {
                if let Some(exit_status) =
                    individual_messages_client(&mut os_input, action, pane_id)
                {
                    return exit_status;
                }
            },
        }
    }
    os_input.send_to_server(ClientToServerMsg::ClientExited);
    0
}

fn pipe_client(
    os_input: &mut Box<dyn ClientOsApi>,
    pipe_id: String,
    mut name: Option<String>,
    mut payload: Option<String>,
    plugin: Option<String>,
    args: Option<BTreeMap<String, String>>,
    mut configuration: Option<BTreeMap<String, String>>,
    launch_new: bool,
    skip_cache: bool,
    floating: Option<bool>,
    in_place: Option<bool>,
    pane_id: Option<u32>,
    cwd: Option<PathBuf>,
    pane_title: Option<String>,
) {
    let mut stdin = os_input.get_stdin_reader();
    let name = name
        // first we try to take the explicitly supplied message name
        .take()
        // then we use the plugin, to facilitate using aliases
        .or_else(|| plugin.clone())
        // then we use a uuid to at least have some sort of identifier for this message
        .or_else(|| Some(Uuid::new_v4().to_string()));
    if launch_new {
        // we do this to make sure the plugin is unique (has a unique configuration parameter) so
        // that a new one would be launched, but we'll still send it to the same instance rather
        // than launching a new one in every iteration of the loop
        configuration
            .get_or_insert_with(BTreeMap::new)
            .insert("_zellij_id".to_owned(), Uuid::new_v4().to_string());
    }
    let create_msg = |payload: Option<String>| -> ClientToServerMsg {
        ClientToServerMsg::Action {
            action: Action::CliPipe {
                pipe_id: pipe_id.clone(),
                name: name.clone(),
                payload,
                args: args.clone(),
                plugin: plugin.clone(),
                configuration: configuration.clone(),
                floating,
                in_place,
                launch_new,
                skip_cache,
                cwd: cwd.clone(),
                pane_title: pane_title.clone(),
            },
            terminal_id: pane_id,
            client_id: None,
            is_cli_client: true,
        }
    };
    let is_piped = !os_input.stdin_is_terminal();
    loop {
        if let Some(payload) = payload.take() {
            let msg = create_msg(Some(payload));
            os_input.send_to_server(msg);
        } else if !is_piped {
            // here we send an empty message to trigger the plugin, because we don't have any more
            // data
            let msg = create_msg(None);
            os_input.send_to_server(msg);
        } else {
            // we didn't get payload from the command line, meaning we listen on STDIN because this
            // signifies the user is about to pipe more (eg. cat my-large-file | zellij pipe ...)
            let mut buffer = String::new();
            let _ = stdin.read_line(&mut buffer);
            if buffer.is_empty() {
                let msg = create_msg(None);
                os_input.send_to_server(msg);
                break;
            } else {
                // we've got data! send it down the pipe (most common)
                let msg = create_msg(Some(buffer));
                os_input.send_to_server(msg);
            }
        }
        loop {
            // wait for a response and act accordingly
            match os_input.recv_from_server() {
                Some((ServerToClientMsg::UnblockCliPipeInput { pipe_name }, _)) => {
                    // unblock this pipe, meaning we need to stop waiting for a response and read
                    // once more from STDIN
                    if pipe_name == pipe_id {
                        if !is_piped {
                            // if this client is not piped, we need to exit the process completely
                            // rather than wait for more data
                            process::exit(0);
                        } else {
                            break;
                        }
                    }
                },
                Some((ServerToClientMsg::CliPipeOutput { pipe_name, output }, _)) => {
                    // send data to STDOUT, this *does not* mean we need to unblock the input
                    let err_context = "Failed to write to stdout";
                    if pipe_name == pipe_id {
                        let mut stdout = os_input.get_stdout_writer();
                        stdout
                            .write_all(output.as_bytes())
                            .context(err_context)
                            .non_fatal();
                        stdout.flush().context(err_context).non_fatal();
                    }
                },
                Some((ServerToClientMsg::Log { lines: log_lines }, _)) => {
                    log_lines.iter().for_each(|line| println!("{line}"));
                    process::exit(0);
                },
                Some((ServerToClientMsg::LogError { lines: log_lines }, _)) => {
                    log_lines.iter().for_each(|line| eprintln!("{line}"));
                    process::exit(2);
                },
                Some((ServerToClientMsg::Exit { exit_reason }, _)) => match exit_reason {
                    ExitReason::Error(e) => {
                        eprintln!("{}", e);
                        process::exit(2);
                    },
                    _ => {
                        process::exit(0);
                    },
                },
                _ => {},
            }
        }
    }
}

fn individual_messages_client(
    os_input: &mut Box<dyn ClientOsApi>,
    action: Action,
    pane_id: Option<u32>,
) -> Option<i32> {
    let is_blocking = matches!(
        &action,
        Action::NewBlockingPane {
            unblock_condition: Some(_),
            ..
        }
    );
    let msg = ClientToServerMsg::Action {
        action,
        terminal_id: pane_id,
        client_id: None,
        is_cli_client: true,
    };
    os_input.send_to_server(msg);
    loop {
        match os_input.recv_from_server() {
            Some((ServerToClientMsg::UnblockInputThread, _)) if !is_blocking => {
                return None;
            },
            Some((ServerToClientMsg::Log { lines: log_lines }, _)) => {
                log_lines.iter().for_each(|line| println!("{line}"));
                return None;
            },
            Some((ServerToClientMsg::LogError { lines: log_lines }, _)) => {
                log_lines.iter().for_each(|line| eprintln!("{line}"));
                return Some(2);
            },
            Some((ServerToClientMsg::Exit { exit_reason }, _)) => match exit_reason {
                ExitReason::Error(e) => {
                    eprintln!("{}", e);
                    return Some(2);
                },
                ExitReason::CustomExitStatus(exit_status) => {
                    return Some(exit_status);
                },
                _ => {
                    return None;
                },
            },
            _ => {},
        }
    }
}

pub fn start_subscribe_client(
    os_input: Box<dyn ClientOsApi>,
    session_name: &str,
    subscribe_cli: SubscribeCli,
) {
    let zellij_ipc_pipe: PathBuf = {
        let mut sock_dir = zellij_utils::consts::ZELLIJ_SOCK_DIR.clone();
        fs::create_dir_all(&sock_dir).unwrap();
        zellij_utils::shared::set_permissions(&sock_dir, 0o700).unwrap();
        sock_dir.push(session_name);
        sock_dir
    };
    crate::check_ipc_pipe_length(&zellij_ipc_pipe);
    os_input.connect_to_server(&*zellij_ipc_pipe);

    // Parse pane IDs
    let pane_ids: Vec<PaneId> = subscribe_cli
        .pane_id
        .iter()
        .map(|s| {
            PaneId::from_str(s).unwrap_or_else(|e| {
                eprintln!("Invalid pane ID '{}': {}", s, e);
                process::exit(2);
            })
        })
        .collect();

    // Send subscribe message
    os_input.send_to_server(ClientToServerMsg::SubscribeToPaneRenders {
        pane_ids: pane_ids.clone(),
        scrollback: subscribe_cli.scrollback,
        ansi: subscribe_cli.ansi,
    });

    // Track remaining panes for exit-on-all-closed
    let mut remaining_panes: HashSet<PaneId> = pane_ids.into_iter().collect();

    // Streaming receive loop
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    loop {
        match os_input.recv_from_server() {
            Some((
                ServerToClientMsg::PaneRenderUpdate {
                    pane_id,
                    viewport,
                    scrollback,
                    is_initial,
                },
                _,
            )) => match subscribe_cli.format {
                SubscribeFormat::Raw => {
                    if let Some(ref scrollback_lines) = scrollback {
                        for line in scrollback_lines {
                            let _ = writeln!(stdout, "{}", line);
                        }
                    }
                    for line in &viewport {
                        let _ = writeln!(stdout, "{}", line);
                    }
                    let _ = stdout.flush();
                },
                SubscribeFormat::Json => {
                    let json = serde_json::json!({
                        "event": "pane_update",
                        "pane_id": pane_id.to_string(),
                        "viewport": viewport,
                        "scrollback": scrollback,
                        "is_initial": is_initial,
                    });
                    let _ = writeln!(stdout, "{}", json);
                    let _ = stdout.flush();
                },
            },
            Some((ServerToClientMsg::SubscribedPaneClosed { pane_id }, _)) => {
                remaining_panes.remove(&pane_id);
                match subscribe_cli.format {
                    SubscribeFormat::Raw => {},
                    SubscribeFormat::Json => {
                        let json = serde_json::json!({
                            "event": "pane_closed",
                            "pane_id": pane_id.to_string(),
                        });
                        let _ = writeln!(stdout, "{}", json);
                        let _ = stdout.flush();
                    },
                }
                if remaining_panes.is_empty() {
                    break;
                }
            },
            Some((ServerToClientMsg::Exit { .. }, _)) => break,
            Some((ServerToClientMsg::LogError { lines }, _)) => {
                for line in lines {
                    eprintln!("{}", line);
                }
                process::exit(2);
            },
            None => break,
            _ => {},
        }
    }

    os_input.send_to_server(ClientToServerMsg::ClientExited);
}
