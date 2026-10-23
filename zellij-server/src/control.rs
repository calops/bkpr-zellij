//! Same-user socket control, sharing native routing and completion acknowledgements.
use crate::{
    os_input_output::ServerOsApi, pane_context, panes::PaneId,
    pty::PtyInstruction,
    route::{route_action, ActionCompletionResult, NotificationEnd, wait_for_action_completion},
    screen::ScreenInstruction, thread_bus::ThreadSenders, ClientId, ServerInstruction,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};
use zellij_utils::{
    cli::CliAction,
    control::{parse_action, ControlRequest, ControlResponse, MAX_REQUEST_BYTES, PROTOCOL},
    data::{Direction, GetPaneCwdResponse, InputMode},
    input::{actions::Action, command::TerminalAction},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionArguments { argv: Vec<String> }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientArguments { client_id: ClientId }

const METHODS: &[&str] = &[
    "capabilities", "snapshot", "action", "input", "session.stop", "client.detach",
    "pane.set_state", "pane.move_to_tab", "pane.swap", "tab.set_sync", "client.focus",
    "client.mode", "layout.apply", "workspace.rename", "workspace.close", "session.rename",
];

const ACTIONS: &[&str] = &[
    "resize", "move-pane", "move-pane-backwards", "clear", "dump-screen", "edit-scrollback",
    "scroll-up", "scroll-down", "scroll-to-bottom", "scroll-to-top", "page-scroll-up",
    "page-scroll-down", "half-page-scroll-up", "half-page-scroll-down",
    "toggle-fullscreen", "toggle-no-ui-fullscreen", "toggle-pane-embed-or-floating",
    "close-pane", "rename-pane", "undo-rename-pane", "toggle-pane-pinned", "set-pane-color",
    "change-floating-pane-coordinates", "toggle-pane-borderless", "set-pane-borderless",
    "toggle-active-sync-tab", "show-floating-panes",
    "hide-floating-panes", "are-floating-panes-visible", "close-tab", "rename-tab",
    "undo-rename-tab", "move-tab", "previous-swap-layout", "next-swap-layout",
    "close-tab-by-id", "rename-tab-by-id", "new-pane", "new-tab", "edit", "stack-panes",
    "dump-layout", "save-session", "list-panes", "list-tabs", "query-tab-names",
];

pub(crate) fn handle(
    request: ControlRequest,
    senders: ThreadSenders,
    default_shell: Option<TerminalAction>,
    default_mode: InputMode,
    os_input: Box<dyn ServerOsApi>,
) -> ControlResponse {
    let fail = |code, message: String| ControlResponse::failed(&request, code, message);
    if request.protocol != PROTOCOL || request.request_id.is_empty() || request.request_id.len() > 256 {
        return fail("invalid_request", "unsupported protocol or invalid request_id".into());
    }
    if !request.arguments.is_object() {
        return fail("invalid_arguments", "arguments must be an object".into());
    }
    if let Err(error) = pane_context::validate_control_context(&request.session_id, request.source_pane_id.as_deref()) {
        return fail("session_mismatch", error);
    }
    match request.method.as_str() {
        "capabilities" => {
            if request.arguments != json!({}) { return fail("invalid_arguments", "capabilities takes no arguments".into()); }
            ControlResponse::completed(&request, json!({
                "protocol": PROTOCOL, "methods": METHODS, "max_request_bytes": MAX_REQUEST_BYTES,
                "action_parser": "native-cli-action", "explicit_targets": true,
                "creation_focus": false, "atomic_input_submit": true,
                "output_subscription": "zellij subscribe", "session_rename": true,
                "tiled_absolute_resize": false,
                "actions": ACTIONS, "relative_split_directions": ["right", "down"],
            }))
        },
        "snapshot" => {
            if request.arguments != json!({}) { return fail("invalid_arguments", "snapshot takes no arguments".into()); }
            let response = screen_request(&request, &senders);
            if response.outcome != "completed" { return response; }
            match pane_context::control_snapshot(&request.session_id, request.source_pane_id.as_deref()) {
                Ok(mut snapshot) => {
                    snapshot["native"] = response.result.unwrap_or(Value::Null);
                    ControlResponse::completed(&request, snapshot)
                },
                Err(error) => fail("session_mismatch", error),
            }
        },
        "action" => {
            let arguments: ActionArguments = match serde_json::from_value(request.arguments.clone()) {
                Ok(arguments) => arguments,
                Err(error) => return fail("invalid_arguments", error.to_string()),
            };
            let cli_action = match parse_action(&arguments.argv) {
                Ok(action) => action,
                Err(error) => return fail("invalid_action", error),
            };
            let snapshot = match pane_context::control_snapshot(&request.session_id, request.source_pane_id.as_deref()) {
                Ok(snapshot) => snapshot,
                Err(error) => return fail("session_mismatch", error),
            };
            if let Err(error) = validate_action(&cli_action, &request, &snapshot) {
                return fail("invalid_target", error);
            }
            let creation = matches!(cli_action, CliAction::NewPane {..} | CliAction::NewTab {..} | CliAction::Edit {..});
            let state_query = matches!(cli_action, CliAction::AreFloatingPanesVisible {..});
            let visibility_setter = matches!(cli_action, CliAction::ShowFloatingPanes {..} | CliAction::HideFloatingPanes {..});
            let layout_dump = matches!(cli_action, CliAction::DumpLayout);
            let cwd = if creation {
                match source_cwd(&request, &senders) {
                    Ok(cwd) => cwd,
                    Err(error) => return fail("source_unavailable", error),
                }
            } else { PathBuf::from("/") };
            let actions = match Action::actions_from_cli(cli_action, Box::new(move || cwd.clone()), None) {
                Ok(actions) => actions,
                Err(error) => return fail("invalid_action", error.to_string()),
            };
            // No last-active-client fallback. Purely targeted actions use the control connection's
            // non-attached identity (0); view operations are separate explicit-client methods.
            let client_id = 0;
            let pane_id = request.source_pane_id.as_deref().and_then(parse_terminal);
            let mut results = Vec::with_capacity(actions.len());
            let _output_scope = crate::route::capture_control_output();
            for action in actions {
                match route_action(action, client_id, None, pane_id, senders.clone(), default_shell.clone(), None, default_mode, Some(os_input.clone())) {
                    Ok((_, Some(mut result))) if result.completed => {
                        if let Some(error) = result.error_message { return fail("native_action_failed", error); }
                        if layout_dump {
                            if let Some(output) = result.stdout_message.as_mut() {
                                match sanitize_layout_output(output) {
                                    Ok(sanitized) => *output = sanitized,
                                    Err(error) => return fail("layout_export_failed", error),
                                }
                            }
                        }
                        if creation && result.affected_pane_id.is_none() && result.affected_tab_id.is_none() {
                            return ControlResponse::unknown(&request, "creation acknowledgement did not identify an installed pane/tab; do not replay automatically");
                        }
                        if let Some(status) = result.exit_status {
                            if status != 0 && !(state_query && status == 1) && !(visibility_setter && status == 2) {
                                let mut response = fail("native_exit_status", format!("native action failed with exit status {status}"));
                                response.result = Some(json!({"exit_status":status,"stdout":result.stdout_message}));
                                return response;
                            }
                        }
                        results.push(json!({
                            "pane_id": result.affected_pane_id.map(|id| id.to_string()),
                            "tab_id": result.affected_tab_id,
                            "exit_status": result.exit_status,
                            "stdout": result.stdout_message,
                        }));
                    },
                    Ok(_) => return ControlResponse::unknown(&request, "native action completion was not acknowledged; do not replay automatically"),
                    Err(error) => return ControlResponse::unknown(&request, format!("native routing failed after dispatch may have begun: {error}")),
                }
            }
            if results.len() == 1 { ControlResponse::completed(&request, results.remove(0)) }
            else { ControlResponse::completed(&request, json!({"actions": results})) }
        },
        "client.detach" => {
            let arguments: ClientArguments = match serde_json::from_value(request.arguments.clone()) {
                Ok(arguments) => arguments,
                Err(error) => return fail("invalid_arguments", error.to_string()),
            };
            // Screen validates attached client identity before this instruction is sent.
            let response = screen_request(&request, &senders);
            if response.outcome != "completed" { return response; }
            let (tx, rx) = tokio::sync::oneshot::channel();
            if let Err(error) = senders.send_to_server(ServerInstruction::DetachSession(vec![arguments.client_id], Some(NotificationEnd::new(tx)))) {
                return fail("dispatch_failed", error.to_string());
            }
            completion_response(&request, wait_for_action_completion(rx, "bkpr-client-detach", false))
        },
        "session.stop" => {
            if request.arguments != json!({}) { return fail("invalid_arguments", "session.stop takes no arguments".into()); }
            // Route writes this accepted-dispatch receipt before queueing shutdown,
            // so normal server teardown cannot swallow its own control response.
            let mut response = ControlResponse::completed(&request, json!({"accepted":true}));
            response.outcome = "dispatched".into();
            response
        },
        method if METHODS.contains(&method) => screen_request(&request, &senders),
        _ => fail("unsupported_method", format!("unsupported control method: {}", request.method)),
    }
}

fn completion_response(request: &ControlRequest, result: ActionCompletionResult) -> ControlResponse {
    if !result.completed { return ControlResponse::unknown(request, "native completion was not acknowledged; do not replay automatically"); }
    if let Some(error) = result.error_message { return ControlResponse::failed(request, "native_action_failed", error); }
    ControlResponse::completed(request, json!({"exit_status":result.exit_status}))
}

fn screen_request(request: &ControlRequest, senders: &ThreadSenders) -> ControlResponse {
    let (tx, rx) = crossbeam::channel::bounded(1);
    if let Err(error) = senders.send_to_screen(ScreenInstruction::BkprControl { request: request.clone(), response: tx }) {
        return ControlResponse::failed(request, "dispatch_failed", error.to_string());
    }
    rx.recv_timeout(Duration::from_secs(4)).unwrap_or_else(|_| ControlResponse::unknown(request, "native screen completion was not acknowledged; do not replay automatically"))
}

fn parse_terminal(id: &str) -> Option<PaneId> {
    Some(PaneId::Terminal(id.strip_prefix("terminal_")?.parse().ok()?))
}

fn validate_action(action: &CliAction, request: &ControlRequest, snapshot: &Value) -> Result<(), String> {
    let pane = |id: &Option<String>| -> Result<(), String> {
        let id = id.as_ref().ok_or("action requires an explicit pane_id")?;
        if id.strip_prefix("plugin_").and_then(|id| id.parse().ok())
            .is_some_and(|id| pane_context::is_owner_pane(PaneId::Plugin(id)))
        {
            return Err("the native Bkpr sidebar is protected from pane mutations".into());
        }
        if snapshot["panes"].as_array().is_some_and(|panes| panes.iter().any(|pane| pane["pane_id"].as_str() == Some(id))) {
            Ok(())
        } else { Err("target pane is not live in the expected session".into()) }
    };
    let tab = |id: Option<usize>| -> Result<(), String> {
        let id = id.ok_or("action requires an explicit tab_id")?;
        if snapshot["tabs"].as_array().is_some_and(|tabs| tabs.iter().any(|tab| tab["tab_id"].as_u64() == Some(id as u64))) {
            Ok(())
        } else { Err("target tab is not live in the expected session".into()) }
    };
    use CliAction::*;
    match action {
        Resize {pane_id,..} | MovePane {pane_id,..} | MovePaneBackwards {pane_id}
        | Clear {pane_id} | DumpScreen {pane_id,..} | EditScrollback {pane_id,..}
        | ScrollUp {pane_id} | ScrollDown {pane_id} | ScrollToBottom {pane_id}
        | ScrollToTop {pane_id} | PageScrollUp {pane_id} | PageScrollDown {pane_id}
        | HalfPageScrollUp {pane_id} | HalfPageScrollDown {pane_id}
        | ToggleFullscreen {pane_id} | ToggleNoUiFullscreen {pane_id}
        | TogglePaneEmbedOrFloating {pane_id} | ClosePane {pane_id}
        | RenamePane {pane_id,..} | UndoRenamePane {pane_id} | TogglePanePinned {pane_id}
        | SetPaneColor {pane_id,..} => pane(pane_id),
        ChangeFloatingPaneCoordinates {pane_id,..} | TogglePaneBorderless {pane_id}
        | SetPaneBorderless {pane_id,..} => pane(&Some(pane_id.clone())),
        ToggleActiveSyncTab {tab_id}
        | ShowFloatingPanes {tab_id} | HideFloatingPanes {tab_id} | AreFloatingPanesVisible {tab_id}
        | CloseTab {tab_id} | RenameTab {tab_id,..} | UndoRenameTab {tab_id}
        | MoveTab {tab_id,..} | PreviousSwapLayout {tab_id} | NextSwapLayout {tab_id} => tab(*tab_id),
        CloseTabById {id} | RenameTabById {id,..} => tab(usize::try_from(*id).ok()),
        NewPane { no_focus, tab_id, pane_id, in_place, near_current_pane, plugin, blocking, direction, stacked,
            block_until_exit, block_until_exit_success, block_until_exit_failure, .. } => {
            if matches!(direction, Some(Direction::Left | Direction::Up)) {
                return Err("relative creation supports native right/down splits only".into());
            }
            if tab_id.is_some() && (direction.is_some() || *stacked) {
                return Err("relative split/stack requires source_pane_id without tab_id".into());
            }
            if !no_focus || *blocking || *block_until_exit || *block_until_exit_success || *block_until_exit_failure {
                return Err("control creation requires --no-focus and forbids blocking completion conditions".into());
            }
            if *in_place { return pane(pane_id); }
            if let Some(id) = tab_id { tab(Some(*id)) }
            else if plugin.is_some() { Err("plugin creation requires explicit tab_id".into()) }
            else if *near_current_pane || *no_focus {
                pane(&request.source_pane_id)
            } else { Err("creation requires a source pane or explicit tab_id".into()) }
        },
        NewTab {no_focus,block_until_exit,block_until_exit_success,block_until_exit_failure,..} => {
            if *no_focus && !block_until_exit && !block_until_exit_success && !block_until_exit_failure { Ok(()) }
            else { Err("tab creation requires --no-focus without blocking completion conditions".into()) }
        },
        Edit {no_focus,tab_id,direction,..} => {
            if matches!(direction, Some(Direction::Left | Direction::Up)) {
                return Err("relative editor creation supports native right/down splits only".into());
            }
            if !no_focus { return Err("editor creation requires --no-focus".into()); }
            if tab_id.is_some() { tab(*tab_id) } else { pane(&request.source_pane_id) }
        },
        StackPanes {pane_ids} => {
            for id in pane_ids { pane(&Some(id.clone()))?; }
            Ok(())
        },
        DumpLayout | SaveSession | ListPanes {..} | ListTabs {..} | QueryTabNames => Ok(()),
        _ => Err("action is not in the safe targetable control surface; use an explicit structured method for client views, state or terminal input".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> ControlRequest {
        ControlRequest {protocol:1,request_id:"test".into(),session_id:"session".into(),source_pane_id:Some("terminal_2".into()),client_id:None,method:"action".into(),arguments:json!({})}
    }
    fn validate(argv: &[&str], request: &ControlRequest) -> Result<(), String> {
        let action = parse_action(&argv.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())?;
        validate_action(&action, request, &json!({"panes":[{"pane_id":"terminal_2"}],"tabs":[{"tab_id":7}]}))
    }
    #[test]
    fn refuses_focus_fallback_and_stale_targets() {
        assert!(validate(&["close-pane"], &request()).is_err());
        assert!(validate(&["close-pane","--pane-id","terminal_99"], &request()).is_err());
        assert!(validate(&["close-pane","--pane-id","terminal_2"], &request()).is_ok());
        assert!(validate(&["switch-mode","locked"], &request()).is_err());
    }
    #[test]
    fn creation_never_borrows_human_focus_or_waits_for_process_exit() {
        assert!(validate(&["new-pane"], &request()).is_err());
        assert!(validate(&["new-pane","--no-focus"], &request()).is_ok());
        let mut outside = request(); outside.source_pane_id = None;
        assert!(validate(&["new-pane","--no-focus"], &outside).is_err());
        assert!(validate(&["new-pane","--no-focus","--tab-id","7"], &outside).is_ok());
        assert!(validate(&["new-pane","--no-focus","--blocking"], &request()).is_err());
    }
    #[test]
    fn session_rename_and_permission_configuration_are_not_control_actions() {
        assert!(validate(&["rename-session","other"], &request()).is_err());
        assert!(validate(&["pipe","--name","bkpr_context"], &request()).is_err());
    }
}

#[cfg(test)]
mod completion_tests {
    use super::*;

    fn request() -> ControlRequest {
        ControlRequest {protocol:1,request_id:"ack".into(),session_id:"incarnation".into(),source_pane_id:None,client_id:None,method:"action".into(),arguments:json!({})}
    }

    #[test]
    fn closed_completion_channel_is_unknown_not_success() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        drop(tx);
        let result = wait_for_action_completion(rx, "closed-control-test", false);
        assert!(!result.completed);
        assert_eq!(completion_response(&request(), result).outcome, "unknown");
    }

    #[test]
    fn real_empty_ack_is_completed_and_native_error_is_failed() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        drop(NotificationEnd::new(tx));
        let result = wait_for_action_completion(rx, "ack-control-test", false);
        assert!(result.completed);
        assert_eq!(completion_response(&request(), result).outcome, "completed");
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut acknowledgement = NotificationEnd::new(tx);
        acknowledgement.set_error_message("target disappeared".into());
        drop(acknowledgement);
        let result = wait_for_action_completion(rx, "error-control-test", false);
        assert_eq!(completion_response(&request(), result).outcome, "failed");
    }
}

fn source_cwd(request: &ControlRequest, senders: &ThreadSenders) -> Result<PathBuf, String> {
    let Some(source) = request.source_pane_id.as_deref().and_then(parse_terminal) else {
        return std::env::current_dir().map_err(|error| error.to_string());
    };
    let (tx, rx) = crossbeam::channel::bounded(1);
    senders.send_to_pty(PtyInstruction::GetPaneCwd {pane_id:source,response_channel:tx})
        .map_err(|error| error.to_string())?;
    match rx.recv_timeout(Duration::from_secs(1)).map_err(|error| error.to_string())? {
        GetPaneCwdResponse::Ok(cwd) => Ok(cwd),
        GetPaneCwdResponse::Err(error) => Err(error),
    }
}

fn sanitize_layout_output(output: &str) -> Result<String, String> {
    fn scrub(document: &mut kdl::KdlDocument) {
        document.nodes_mut().retain(|node| node.name().value() != "context_owner");
        for node in document.nodes_mut() {
            if let Some(children) = node.children_mut() { scrub(children); }
        }
    }
    let mut document: kdl::KdlDocument = output.parse().map_err(|error| format!("invalid native layout output: {error}"))?;
    scrub(&mut document);
    Ok(document.to_string())
}

#[cfg(test)]
mod layout_export_tests {
    use super::*;
    #[test]
    fn exported_layout_omits_owner_authority_but_preserves_user_configuration() {
        let output = sanitize_layout_output("layout { pane { plugin location=\"file:/bkpr.wasm\" { context_owner \"private-token\"; role \"sidebar\"; user_setting \"keep\"; } } }").unwrap();
        assert!(!output.contains("private-token"));
        assert!(!output.contains("context_owner"));
        assert!(output.contains("user_setting"));
        assert!(output.contains("keep"));
        assert!(output.contains("sidebar"));
    }
}

#[cfg(test)]
mod output_capture_tests {
    use super::*;

    #[test]
    fn query_output_capture_follows_original_completion_across_threads_only() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let acknowledgement = {
            let _scope = crate::route::capture_control_output();
            NotificationEnd::new(tx)
        };
        assert!(acknowledgement.capture_output());
        let (ordinary_tx, _ordinary_rx) = tokio::sync::oneshot::channel();
        assert!(!NotificationEnd::new(ordinary_tx).capture_output());
        std::thread::spawn(move || {
            let mut acknowledgement = acknowledgement;
            assert!(acknowledgement.capture_output());
            acknowledgement.set_stdout_message("actual pane viewport".into());
        }).join().unwrap();
        let result = wait_for_action_completion(rx, "captured-output-test", false);
        assert!(result.completed);
        assert_eq!(result.stdout_message.as_deref(), Some("actual pane viewport"));
    }
}
