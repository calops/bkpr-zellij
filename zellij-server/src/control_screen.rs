//! Explicit-target controls executed by the native screen owner.
use super::*;
use crossbeam::channel::Sender;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use zellij_utils::control::{ControlRequest, ControlResponse};

pub(super) struct PendingLayout {
    request: ControlRequest,
    response: Sender<ControlResponse>,
    tab_id: usize,
    pane_ids: HashSet<PaneId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PaneState {
    pane_id: String,
    floating: Option<bool>,
    pinned: Option<bool>,
    fullscreen: Option<bool>,
    borderless: Option<bool>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MovePane { pane_id: String, tab_id: usize }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SwapPanes { pane_id: String, other_pane_id: String }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TabSync { tab_id: usize, sync: bool }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientFocus { client_id: ClientId, pane_id: Option<String>, tab_id: Option<usize> }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientMode { client_id: ClientId, mode: String }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientTarget { client_id: ClientId }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyLayout { tab_id: usize, layout: String }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameWorkspace { workspace_id: String, name: String }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseWorkspace { workspace_id: String }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameSession { name: String }
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Input {
    Text { pane_id: String, text: String, #[serde(default)] submit: bool },
    Paste { pane_id: String, text: String, #[serde(default)] submit: bool },
    Keys { pane_id: String, keys: Vec<String>, #[serde(default)] submit: bool },
    Bytes { pane_id: String, bytes: Vec<u8>, #[serde(default)] submit: bool },
}

fn decode<T: DeserializeOwned>(arguments: &Value) -> std::result::Result<T, String> {
    T::deserialize(arguments).map_err(|e| format!("Invalid arguments: {e}"))
}
fn pane_id(value: &str) -> std::result::Result<PaneId, String> {
    let (kind, id) = value.split_once('_').ok_or("Expected terminal_N or plugin_N")?;
    let id = id.parse::<u32>().map_err(|_| "Invalid pane number")?;
    match kind {
        "terminal" => Ok(PaneId::Terminal(id)),
        "plugin" => Ok(PaneId::Plugin(id)),
        _ => Err("Expected terminal_N or plugin_N".into()),
    }
}
fn pane_name(id: PaneId) -> String {
    match id {
        PaneId::Terminal(id) => format!("terminal_{id}"),
        PaneId::Plugin(id) => format!("plugin_{id}"),
    }
}

fn is_sidebar_run(run: &Option<Run>) -> bool {
    let Some(owner) = crate::pane_context::owner_plugin_location() else { return false; };
    match run {
        Some(Run::Plugin(RunPluginOrAlias::RunPlugin(plugin))) => plugin.location == owner,
        Some(Run::Plugin(RunPluginOrAlias::Alias(alias))) =>
            alias.run_plugin.as_ref().is_some_and(|plugin| plugin.location == owner),
        _ => false,
    }
}

fn protect_sidebar_layout(tab: &Tab, tiled: &mut TiledPaneLayout, floating: &[FloatingPaneLayout]) -> std::result::Result<(), String> {
    // Callers cannot supply a replacement instance/configuration of the Bkpr plugin.
    if tiled.extract_run_instructions().iter().any(is_sidebar_run)
        || floating.iter().any(|layout| is_sidebar_run(&layout.run)) {
        return Err("Layout must not instantiate or reconfigure the Bkpr sidebar plugin".into());
    }
    let mut sidebars = tab.get_tiled_panes().filter(|(_, pane)| is_sidebar_run(pane.invoked_with()));
    let Some((_, sidebar)) = sidebars.next() else { return Ok(()); };
    if sidebars.next().is_some() { return Err("Target tab has ambiguous sidebar geometry".into()); }
    let geom = sidebar.position_and_size();
    if geom.x != 0 || geom.y != 0 || geom.rows.as_usize() != tab.size.rows || geom.cols.as_usize() >= tab.size.cols {
        return Err("Cannot retain a sidebar that is not a full-height left native pane".into());
    }
    let sidebar_layout = TiledPaneLayout {
        run: sidebar.invoked_with().clone(),
        borderless: Some(sidebar.borderless()),
        split_size: Some(zellij_utils::input::layout::SplitSize::Fixed(geom.cols.as_usize())),
        ..Default::default()
    };
    let content = std::mem::take(tiled);
    *tiled = TiledPaneLayout {
        children_split_direction: zellij_utils::input::layout::SplitDirection::Vertical,
        children: vec![sidebar_layout, content],
        ..Default::default()
    };
    Ok(())
}

fn retain_unmatched_tiled_panes(tab: &Tab, layout: &mut TiledPaneLayout) {
    let mut requested = layout.extract_run_instructions();
    let mut retained = Vec::new();
    for (_, pane) in tab.get_tiled_panes() {
        if is_sidebar_run(pane.invoked_with()) { continue; }
        if let Some(index) = requested.iter().position(|run| run == pane.invoked_with()) {
            requested.remove(index);
        } else {
            retained.push(TiledPaneLayout {
                run: pane.invoked_with().clone(),
                borderless: Some(pane.borderless()),
                ..Default::default()
            });
        }
    }
    if !retained.is_empty() {
        retained.insert(0, std::mem::take(layout));
        *layout = TiledPaneLayout {
            children_split_direction: zellij_utils::input::layout::SplitDirection::Vertical,
            children: retained,
            ..Default::default()
        };
    }
}

fn prepare_control_layout(
    tab: &Tab, layout: &mut TiledPaneLayout, floating: &[FloatingPaneLayout],
    context: &crate::pane_context::TabContext,
) -> std::result::Result<(), String> {
    fn strip_metadata(layout: &mut TiledPaneLayout) {
        layout.context_metadata.clear();
        for child in &mut layout.children { strip_metadata(child); }
    }
    strip_metadata(layout);
    retain_unmatched_tiled_panes(tab, layout);
    protect_sidebar_layout(tab, layout, floating)?;
    layout.context_metadata.insert("bkpr_workspace_id".into(), context.workspace_id.clone());
    layout.context_metadata.insert("bkpr_workspace_name".into(), context.workspace_name.clone());
    Ok(())
}

impl Screen {
    pub(crate) fn native_control_snapshot(&self) -> Value {
        let tabs: Vec<Value> = self.tabs.values().map(|tab| json!({
            "tab_id": tab.id, "position": tab.position, "name": tab.name,
            "sync": tab.is_sync_panes_active(),
            "floating_visible": tab.are_floating_panes_visible(),
            "fullscreen_pane_id": tab.fullscreen_pane_id().map(pane_name),
        })).collect();
        let panes: Vec<Value> = self.tabs.values().flat_map(|tab| {
            tab.pane_infos().into_iter().map(move |info| {
                let id = if info.is_plugin { PaneId::Plugin(info.id) } else { PaneId::Terminal(info.id) };
                let pane = tab.get_pane_with_id(id);
                json!({"pane_id": pane_name(id), "tab_id": tab.id,
                    "floating": tab.pane_id_is_floating(&id),
                    "protected": pane.is_some_and(|p| is_sidebar_run(p.invoked_with())),
                    "fullscreen": tab.fullscreen_pane_id() == Some(id),
                    "pinned": pane.map(|p| p.current_geom().is_pinned),
                    "borderless": pane.map(|p| p.borderless()), "native": info})
            })
        }).collect();
        let clients: Vec<Value> = self.connected_clients.borrow().iter().map(|(id, web)| json!({
            "client_id": id, "is_web_client": web,
            "tab_id": self.active_tab_ids.get(id),
            "pane_id": self.get_active_pane_id(id).map(pane_name),
            "mode": self.mode_info.get(id).map(|m| m.mode),
            "size": self.client_sizes.get(id).map(|s| json!({"rows":s.rows,"columns":s.cols})),
        })).collect();
        json!({"panes": panes, "tabs": tabs, "clients": clients})
    }

    pub(crate) fn handle_bkpr_control(&mut self, request: ControlRequest, response: Sender<ControlResponse>) {
        if request.method == "layout.apply" {
            if let Err(error) = self.control_start_layout(&request, response.clone()) {
                let _ = response.send(ControlResponse::failed(&request, "invalid_state", error));
            }
            return;
        }
        if request.method == "client.mode" {
            if let Err(error) = self.control_start_mode(&request, response.clone()) {
                let _ = response.send(ControlResponse::failed(&request, "invalid_state", error));
            }
            return;
        }
        let result = self.control_mutation(&request);
        let reply = match result {
            Ok(result) => {
                // Mutation receipts do not depend on rendering or plugin notification success.
                if request.method != "snapshot" {
                    self.log_and_report_session_state().non_fatal();
                    self.render(None).non_fatal();
                }
                ControlResponse::completed(&request, result)
            },
            Err(error) => ControlResponse::failed(&request, "invalid_state", error),
        };
        let _ = response.send(reply);
    }

    fn control_live_tab(&self, id: PaneId) -> std::result::Result<usize, String> {
        self.tabs.iter().find(|(_, tab)| tab.has_non_suppressed_pane_with_pid(&id))
            .map(|(id, _)| *id).ok_or_else(|| "Pane does not exist or is suppressed".into())
    }
    fn control_client(&self, id: ClientId) -> std::result::Result<(), String> {
        if self.connected_clients.borrow().contains_key(&id) && self.active_tab_ids.contains_key(&id) {
            Ok(())
        } else { Err("Client is not attached".into()) }
    }

    fn control_unprotected(&self, id: PaneId) -> std::result::Result<(), String> {
        if self.tabs.values().filter_map(|tab| tab.get_pane_with_id(id))
            .any(|pane| is_sidebar_run(pane.invoked_with()))
        {
            Err("The Bkpr sidebar is protected from explicit pane mutations".into())
        } else { Ok(()) }
    }

    fn control_switch_client_tab(&mut self, client_id: ClientId, tab_id: usize) -> std::result::Result<(), String> {
        let source_id = *self.active_tab_ids.get(&client_id).ok_or("Client is not attached")?;
        if source_id == tab_id { return Ok(()); }
        let old_pane = self.get_active_pane_id(&client_id);
        let mode_info = self.mode_info.get(&client_id).cloned().unwrap_or_else(|| self.default_mode_info.clone());
        let destination = self.tabs.get_mut(&tab_id).ok_or("Tab does not exist")?;
        destination.control_add_client(client_id, mode_info).map_err(|error| error.to_string())?;
        destination.update_input_modes().map_err(|error| error.to_string())?;
        destination.visible(true).map_err(|error| error.to_string())?;
        if let Some(source) = self.tabs.get_mut(&source_id) {
            source.drain_single_client(client_id);
            source.clear_mouse_hover_for_client(client_id);
            if source.has_no_connected_clients() {
                source.visible(false).map_err(|error| error.to_string())?;
            }
        }
        self.update_client_tab_focus(client_id, tab_id);
        self.recompute_tab_size(source_id).map_err(|error| error.to_string())?;
        self.recompute_tab_size(tab_id).map_err(|error| error.to_string())?;
        if let (Some(old), Some(new)) = (old_pane, self.get_active_pane_id(&client_id)) {
            self.report_key_passthrough_state(client_id, old, new);
        }
        self.sync_scroll_mode_on_focus(client_id).map_err(|error| error.to_string())?;
        Ok(())
    }

    fn control_start_mode(&self, request: &ControlRequest, response: Sender<ControlResponse>) -> std::result::Result<(), String> {
        let args: ClientMode = decode(&request.arguments)?;
        self.control_client(args.client_id)?;
        let mode = args.mode.parse::<InputMode>().map_err(|e| e.to_string())?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let completion = {
            let _capture = crate::route::capture_control_output();
            NotificationEnd::new(tx)
        };
        self.bus.senders.send_to_server(ServerInstruction::ChangeMode(
            args.client_id, mode, Some(completion),
        )).map_err(|e| e.to_string())?;
        let request = request.clone();
        crate::global_async_runtime::get_tokio_runtime().spawn(async move {
            let reply = match tokio::time::timeout(Duration::from_secs(5), rx).await {
                Ok(Ok(result)) if result.completed && result.error_message.is_none()
                    && result.exit_status.unwrap_or(0) == 0 =>
                    ControlResponse::completed(&request, json!({"client_id":args.client_id,"mode":mode})),
                Ok(Ok(result)) if result.error_message.is_some() || result.exit_status.is_some_and(|s| s != 0) =>
                    ControlResponse::failed(&request, "mode_failed", result.error_message.unwrap_or_else(|| "Native mode update failed".into())),
                _ => ControlResponse::unknown(&request, "Mode completion was not observed; do not replay automatically"),
            };
            let _ = response.send(reply);
        });
        Ok(())
    }

    fn control_mutation(&mut self, request: &ControlRequest) -> std::result::Result<Value, String> {
        let args = &request.arguments;
        match request.method.as_str() {
            "snapshot" => {
                let _: BTreeMap<String, Value> = decode(args)?;
                if !args.as_object().is_some_and(|a| a.is_empty()) { return Err("snapshot takes no arguments".into()); }
                Ok(self.native_control_snapshot())
            },
            "pane.set_state" => {
                let args: PaneState = decode(args)?;
                let id = pane_id(&args.pane_id)?;
                self.control_unprotected(id)?;
                let tab_id = self.control_live_tab(id)?;
                let tab = self.tabs.get_mut(&tab_id).ok_or("Tab disappeared")?;
                tab.control_set_pane_state(id, args.floating, args.pinned, args.fullscreen, args.borderless)
                    .map_err(|e| e.to_string())?;
                Ok(json!({"pane_id":args.pane_id,"tab_id":tab_id}))
            },
            "pane.move_to_tab" => {
                let args: MovePane = decode(args)?;
                let id = pane_id(&args.pane_id)?;
                self.control_unprotected(id)?;
                let source_id = self.control_live_tab(id)?;
                if !self.tabs.contains_key(&args.tab_id) { return Err("Destination tab does not exist".into()); }
                if source_id != args.tab_id {
                    let mut source = self.tabs.remove(&source_id).ok_or("Source tab disappeared")?;
                    let result = match self.tabs.get_mut(&args.tab_id) {
                        Some(destination) => source.control_transfer_pane(destination, id),
                        None => Err(anyhow!("Destination tab disappeared")),
                    };
                    self.tabs.insert(source_id, source);
                    self.synchronize_pane_context();
                    result.map_err(|e| e.to_string())?;
                }
                self.synchronize_pane_context();
                Ok(json!({"pane_id":args.pane_id,"tab_id":args.tab_id,"source_tab_id":source_id}))
            },
            "pane.swap" => {
                let args: SwapPanes = decode(args)?;
                let id = pane_id(&args.pane_id)?;
                let other = pane_id(&args.other_pane_id)?;
                self.control_unprotected(id)?;
                self.control_unprotected(other)?;
                let tab_id = self.control_live_tab(id)?;
                if self.control_live_tab(other)? != tab_id { return Err("Swap requires both panes in the same tab".into()); }
                self.tabs.get_mut(&tab_id).ok_or("Tab disappeared")?.control_swap_panes(id, other).map_err(|e| e.to_string())?;
                Ok(json!({"pane_id":args.pane_id,"other_pane_id":args.other_pane_id,"tab_id":tab_id}))
            },
            "tab.set_sync" => {
                let args: TabSync = decode(args)?;
                let tab = self.tabs.get_mut(&args.tab_id).ok_or("Tab does not exist")?;
                if tab.is_sync_panes_active() != args.sync { tab.toggle_sync_panes_is_active(); }
                Ok(json!({"tab_id":args.tab_id,"sync":tab.is_sync_panes_active()}))
            },
            // Detachment is completed by the server owner after this screen-
            // authoritative validation; this branch itself never detaches.
            "client.detach" => {
                let args: ClientTarget = decode(args)?;
                self.control_client(args.client_id)?;
                Ok(json!({"client_id":args.client_id}))
            },
            "client.focus" => {
                let args: ClientFocus = decode(args)?;
                self.control_client(args.client_id)?;
                if args.pane_id.is_some() == args.tab_id.is_some() { return Err("Specify exactly one of pane_id and tab_id".into()); }
                if self.session_is_mirrored && self.connected_clients.borrow().len() > 1 {
                    return Err("Independent client focus is unavailable in a mirrored session".into());
                }
                let id = args.pane_id.as_deref().map(pane_id).transpose()?;
                let tab_id = match id { Some(id) => self.control_live_tab(id)?, None => args.tab_id.ok_or("Missing tab")? };
                let tab = self.tabs.get(&tab_id).ok_or("Tab does not exist")?;
                if let Some(id) = id {
                    if !tab.get_pane_with_id(id).is_some_and(|p| p.selectable()) { return Err("Pane is not selectable".into()); }
                    // Native floating visibility belongs to the tab, not a client.
                    if tab.are_floating_panes_visible() != tab.pane_id_is_floating(&id)
                        && self.active_tab_ids.iter().any(|(client, active)| *client != args.client_id && *active == tab_id) {
                        return Err("Focusing this pane would change another client's floating visibility".into());
                    }
                }
                self.control_switch_client_tab(args.client_id, tab_id)?;
                if let Some(id) = id {
                    self.tabs.get_mut(&tab_id).ok_or("Tab disappeared")?.control_focus_pane(id, args.client_id).map_err(|e| e.to_string())?;
                    if self.get_active_pane_id(&args.client_id) != Some(id) { return Err("Native focus was not accepted".into()); }
                }
                if self.active_tab_ids.get(&args.client_id) != Some(&tab_id) { return Err("Native tab focus was not accepted".into()); }
                self.synchronize_pane_context();
                Ok(json!({"client_id":args.client_id,"tab_id":tab_id,"pane_id":self.get_active_pane_id(&args.client_id).map(pane_name),"shared_layout_scope":"tab"}))
            },
            "workspace.rename" => {
                let args: RenameWorkspace = decode(args)?;
                crate::pane_context::rename_workspace(&args.workspace_id, &args.name)?;
                self.report_pane_context().non_fatal();
                Ok(json!({"workspace_id":args.workspace_id,"name":args.name}))
            },
            "workspace.close" => {
                let args: CloseWorkspace = decode(args)?;
                self.synchronize_pane_context();
                let tab_ids = crate::pane_context::workspace_tab_ids(&args.workspace_id)?;
                if tab_ids.is_empty() { return Err("Workspace has no live or pending native tabs".into()); }
                for tab_id in &tab_ids {
                    if self.tabs.contains_key(tab_id) {
                        self.close_tab_by_id(*tab_id).map_err(|error| error.to_string())?;
                    } else {
                        crate::pane_context::discard_pending_tab(*tab_id);
                    }
                }
                self.synchronize_pane_context();
                Ok(json!({"workspace_id":args.workspace_id,"count":tab_ids.len(),"tab_ids":tab_ids}))
            },
            "session.rename" => {
                let args: RenameSession = decode(args)?;
                crate::pane_context::rename_session(&args.name)?;
                self.report_pane_context().non_fatal();
                Ok(json!({"name":args.name,"session_id":request.session_id,"native_session_name_unchanged":true}))
            },
            "input" => {
                let args: Input = decode(args)?;
                let (target, submit, parts) = input_parts(args)?;
                let id = pane_id(&target)?;
                if !matches!(id, PaneId::Terminal(_)) { return Err("Input requires a terminal pane".into()); }
                let tab_id = self.control_live_tab(id)?;
                self.tabs.get_mut(&tab_id).ok_or("Tab disappeared")?.control_input(id, parts).map_err(|e| e.to_string())?;
                Ok(json!({"pane_id":target,"accepted_by":"native_pty_writer_queue","submit":submit,"agent_processing_acknowledged":false}))
            },
            _ => Err(format!("Unknown screen control method: {}", request.method)),
        }
    }

    fn control_start_layout(&mut self, request: &ControlRequest, response: Sender<ControlResponse>) -> std::result::Result<(), String> {
        let args: ApplyLayout = decode(&request.arguments)?;
        let tab = self.tabs.get(&args.tab_id).ok_or("Tab does not exist")?;
        let pane_ids: HashSet<PaneId> = tab.get_all_pane_ids().into_iter().collect();
        if self.control_pending_layouts.values().any(|p| p.tab_id == args.tab_id) { return Err("A control layout is already pending for this tab".into()); }
        let context = crate::pane_context::tab_context(args.tab_id).ok_or("Target tab has no authoritative workspace binding")?;
        let mut layout = Layout::from_str(&args.layout, "bkpr-control".into(), None, None).map_err(|e| e.to_string())?;
        for (layouts, _) in &mut layout.swap_tiled_layouts {
            for layout in layouts.values_mut() {
                prepare_control_layout(tab, layout, &[], &context)?;
            }
        }
        for (layouts, _) in &layout.swap_floating_layouts {
            if layouts.values().flatten().any(|layout| is_sidebar_run(&layout.run)) {
                return Err("Swap layouts must not instantiate the Bkpr sidebar plugin".into());
            }
        }
        let mut tabs = layout.tabs();
        if tabs.len() > 1 { return Err("Targeted layout must contain at most one tab".into()); }
        let (_, mut tiled, mut floating) = tabs.pop().unwrap_or_else(|| { let (t, f) = layout.new_tab(); (None, t, f) });
        prepare_control_layout(tab, &mut tiled, &floating, &context)?;
        tiled.position_panes_in_space(&PaneGeom::from(&tab.size), None, false, false)
            .map_err(|error| format!("Layout does not fit the target tab: {error}"))?;
        let (running, floating_running) = find_already_running_panes(&tiled, &floating, tab);
        for run in running { tiled.ignore_run_instruction(run); }
        for index in floating_running { floating[index].already_running = true; }
        // This client is only the existing plugin/PTY pipeline's owner, never a focus target.
        let client_id = request.client_id.or_else(|| self.active_tab_ids.iter().find(|(_, id)| **id == args.tab_id).map(|(id, _)| *id))
            .or_else(|| self.active_tab_ids.keys().next().copied()).ok_or("Layout application requires an attached native client")?;
        self.control_client(client_id)?;
        let marker = format!("bkpr-control-layout:{}", uuid::Uuid::new_v4());
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut completion = {
            let _capture = crate::route::capture_control_output();
            NotificationEnd::new(tx)
        };
        completion.set_control_correlation(marker.clone());
        let tab_layout = TabLayoutInfo { tab_index: args.tab_id, tab_name: None, tiled_layout: tiled, floating_layouts: floating,
            swap_tiled_layouts: Some(layout.swap_tiled_layouts), swap_floating_layouts: Some(layout.swap_floating_layouts) };
        self.control_pending_layouts.insert(marker.clone(), PendingLayout { request: request.clone(), response, tab_id: args.tab_id, pane_ids });
        if let Err(error) = self.bus.senders.send_to_plugin(PluginInstruction::OverrideLayout(None, None, vec![tab_layout], true, true, client_id, Some(completion))) {
            self.control_pending_layouts.remove(&marker);
            return Err(error.to_string());
        }
        let senders = self.bus.senders.clone();
        crate::global_async_runtime::get_tokio_runtime().spawn(async move {
            let error = rx.await.ok().and_then(|result| result.error_message);
            // On the normal path Screen already consumed the correlated completion.
            // If preparation fails before that return, release the pending request honestly.
            let _ = senders.send_to_screen(ScreenInstruction::BkprControlLayoutAborted { marker, error });
        });
        Ok(())
    }

    pub(super) fn control_abort_layout(&mut self, marker: &str, error: Option<String>) {
        if let Some(pending) = self.control_pending_layouts.remove(marker) {
            let reply = match error {
                Some(error) => ControlResponse::failed(&pending.request, "layout_failed", error),
                None => ControlResponse::unknown(&pending.request, "Layout preparation ended without a native mutation completion"),
            };
            let _ = pending.response.send(reply);
        }
    }

    pub(super) fn control_close_layout_resources(&self, results: &[TabOverrideResult]) {
        for result in results {
            for (terminal, _) in result.new_terminal_pids.iter().chain(&result.new_floating_pane_pids) {
                self.bus.senders.send_to_pty(PtyInstruction::ClosePane(PaneId::Terminal(*terminal), None)).non_fatal();
            }
            for plugin in result.plugin_ids.values().flatten() {
                self.bus.senders.send_to_pty(PtyInstruction::ClosePane(PaneId::Plugin(*plugin), None)).non_fatal();
            }
        }
    }

    pub(super) fn control_finish_layout(&mut self, marker: &str, results: Vec<TabOverrideResult>, client_id: ClientId) {
        let Some(pending) = self.control_pending_layouts.remove(marker) else {
            self.control_close_layout_resources(&results);
            return;
        };
        let current_ids = self.tabs.get(&pending.tab_id).map(|tab| tab.get_all_pane_ids());
        let result = if results.len() != 1 || results[0].tab_index != pending.tab_id {
            self.control_close_layout_resources(&results);
            Err("Native layout completion did not match the requested tab".into())
        } else if !self.tabs.contains_key(&pending.tab_id) {
            self.control_close_layout_resources(&results);
            Err("Target tab closed while layout was being prepared".into())
        } else if current_ids.as_ref().is_some_and(|ids| {
            ids.len() != pending.pane_ids.len() || ids.iter().any(|id| !pending.pane_ids.contains(id))
        }) {
            self.control_close_layout_resources(&results);
            Err("Target tab's pane identities changed while layout was being prepared".into())
        } else {
            let result = results.into_iter().next().expect("one layout result");
            self.tabs.get_mut(&pending.tab_id).expect("validated target tab")
                .control_override_layout(result, client_id).map_err(|e| e.to_string())
        };
        self.synchronize_pane_context();
        self.log_and_report_session_state().non_fatal();
        self.render(None).non_fatal();
        let reply = match result {
            Ok(()) => ControlResponse::completed(&pending.request, json!({"tab_id":pending.tab_id,"retained_existing_panes":true})),
            Err(error) => ControlResponse::failed(&pending.request, "layout_failed", error),
        };
        let _ = pending.response.send(reply);
    }
}

// A part is (native key, bytes, bypass terminal preprocessing). Everything is prepared
// before enqueueing one PTY write, so invalid keys cannot leave a partial paste behind.
fn input_parts(input: Input) -> std::result::Result<(String, bool, Vec<(Option<KeyWithModifier>, Vec<u8>, bool)>), String> {
    let mut parts = Vec::new();
    let (pane_id, submit) = match input {
        Input::Text { pane_id, text, submit } => {
            parts.push((None, text.into_bytes(), false));
            (pane_id, submit)
        },
        Input::Paste { pane_id, text, submit } => {
            parts.push((None, BRACKETED_PASTE_BEGIN.to_vec(), false));
            parts.push((None, text.into_bytes(), false));
            parts.push((None, BRACKETED_PASTE_END.to_vec(), false));
            (pane_id, submit)
        },
        Input::Bytes { pane_id, bytes, submit } => {
            parts.push((None, bytes, true));
            (pane_id, submit)
        },
        Input::Keys { pane_id, keys, submit } => {
            parts.reserve(keys.len() + usize::from(submit));
            for key in keys {
                let key = key.parse::<KeyWithModifier>().map_err(|e| e.to_string())?;
                let bytes = key.serialize_non_kitty().ok_or("Key cannot be encoded")?.into_bytes();
                parts.push((Some(key), bytes, false));
            }
            (pane_id, submit)
        },
    };
    if submit {
        let enter = "Enter".parse::<KeyWithModifier>().map_err(|e| e.to_string())?;
        parts.push((Some(enter), vec![13], false));
    }
    Ok((pane_id, submit, parts))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_mixed_input_kinds_before_writing() {
        assert!(decode::<Input>(&json!({"pane_id":"terminal_1","kind":"paste","text":"hello","bytes":[13]})).is_err());
        assert!(decode::<Input>(&json!({"pane_id":"terminal_1","kind":"text","text":"hello","keys":null})).is_err());
    }
    #[test]
    fn paste_and_submit_have_separate_native_framing() {
        let input: Input = decode(&json!({"pane_id":"terminal_1","kind":"paste","text":"hello","submit":true})).unwrap();
        let (_, _, parts) = input_parts(input).unwrap();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0].1, BRACKETED_PASTE_BEGIN);
        assert_eq!(parts[1].1, b"hello");
        assert_eq!(parts[2].1, BRACKETED_PASTE_END);
        assert_eq!(parts[3].1, vec![13]);
    }
    #[test]
    fn requires_typed_stable_target_ids() {
        assert_eq!(pane_id("terminal_8").unwrap(), PaneId::Terminal(8));
        assert_eq!(pane_id("plugin_8").unwrap(), PaneId::Plugin(8));
        assert!(pane_id("8").is_err());
        assert!(pane_id("terminal_-1").is_err());
    }
    #[test]
    fn rejects_unknown_state_arguments() {
        assert!(decode::<PaneState>(&json!({"pane_id":"terminal_1","toggle":true})).is_err());
    }
}
