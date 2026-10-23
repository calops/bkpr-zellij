//! Bkpr's creation-time context and live native topology. No plugin round trip
//! is involved in choosing the environment of a terminal process.
use crate::{panes::PaneId, pty::ClientTabIndexOrPaneId, ClientId};
use serde::{Deserialize, Serialize};
use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    sync::{LazyLock, Mutex},
};
use zellij_utils::{
    data::Event,
    input::layout::{RunPluginLocation, RunPluginOrAlias, TiledPaneLayout},
};

const CONTEXT_ENV: [&str; 5] = [
    "BKPR_SESSION_ID",
    "BKPR_SESSION_NAME",
    "BKPR_WORKSPACE_ID",
    "BKPR_WORKSPACE_NAME",
    "BKPR_PANE_ID",
];

/// Immutable creation-time context; Screen may remove or move the destination
/// after reservation, without changing the environment of this child.
pub(crate) struct TerminalEnvironment {
    session_id: String,
    session_name: String,
    workspace_id: String,
    workspace_name: String,
    pane_id: String,
}

impl TerminalEnvironment {
    pub(crate) fn apply(&self, command: &mut std::process::Command) {
        clear_environment(command);
        command
            .env("BKPR_SESSION_ID", &self.session_id)
            .env("BKPR_SESSION_NAME", &self.session_name)
            .env("BKPR_WORKSPACE_ID", &self.workspace_id)
            .env("BKPR_WORKSPACE_NAME", &self.workspace_name)
            .env("BKPR_PANE_ID", &self.pane_id);
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TabContext {
    pub tab_id: usize,
    pub workspace_id: String,
    pub workspace_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct PaneContext {
    pane_id: String,
    tab_id: usize,
    workspace_id: String,
    workspace_name: String,
    root_pid: Option<u32>,
}

#[derive(Serialize)]
struct Snapshot<'a> {
    version: u32,
    session_id: &'a str,
    session_name: &'a str,
    tabs: Vec<&'a TabContext>,
    panes: Vec<PaneContext>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Update {
    session_id: String,
    generation: u64,
    tabs: Vec<TabContext>,
}

struct TabBinding {
    context: TabContext,
    native_name: String,
    explicit: bool,
}

pub(crate) struct Registry {
    session_id: String,
    session_name: String,
    owner_token: String,
    owner: Option<(u32, u64)>,
    owner_plugin: Option<u32>,
    protected_plugins: BTreeSet<u32>,
    owner_lease: Option<std::fs::File>,
    restart_pending: bool,
    owner_configuration: Option<RunPluginOrAlias>,
    tabs: BTreeMap<usize, TabBinding>,
    next_workspace_id: u64,
    // Layout overrides spawn new tabs before Screen installs their topology.
    pending_tabs: BTreeSet<usize>,
    closed_tabs: BTreeSet<usize>,
    panes: BTreeMap<String, usize>,
    pending_panes: BTreeSet<String>,
    root_pids: BTreeMap<u32, u32>,
    clients: BTreeMap<ClientId, usize>,
}

impl Registry {
    fn new(session_id: String, session_name: String, owner_token: String) -> Self {
        Self {
            session_id,
            session_name,
            owner_token,
            owner: None,
            owner_plugin: None,
            protected_plugins: BTreeSet::new(),
            owner_lease: None,
            restart_pending: false,
            owner_configuration: None,
            tabs: BTreeMap::new(),
            pending_tabs: BTreeSet::new(),
            next_workspace_id: 0,
            closed_tabs: BTreeSet::new(),
            panes: BTreeMap::new(),
            pending_panes: BTreeSet::new(),
            root_pids: BTreeMap::new(),
            clients: BTreeMap::new(),
        }
    }

    fn rename_session(&mut self, name: &str) -> Result<(), String> {
        if name.is_empty() || name.len() > 4096 || name.chars().any(char::is_control) {
            return Err("session label must be nonempty printable text of at most 4096 bytes".into());
        }
        self.session_name = name.into();
        Ok(())
    }

    fn validate_control_context(&self, expected_session: &str, source_pane: Option<&str>) -> Result<(), String> {
        if self.session_id != expected_session {
            return Err("session incarnation does not match expected Bkpr session".into());
        }
        if let Some(source) = source_pane {
            if !source.starts_with("terminal_")
                || !self.panes.contains_key(source)
                || self.pending_panes.contains(source)
            {
                return Err("source pane is not a live terminal in this session".into());
            }
        }
        Ok(())
    }

    fn ensure_tab(&mut self, tab_id: usize, name: &str) {
        if self.tabs.contains_key(&tab_id) {
            return;
        }
        self.closed_tabs.remove(&tab_id);
        let id = format!("{}:workspace:{}", self.session_id, self.next_workspace_id);
        self.next_workspace_id = self
            .next_workspace_id
            .checked_add(1)
            .expect("native workspace identity exhausted");
        self.tabs.insert(
            tab_id,
            TabBinding {
                context: TabContext {
                    tab_id,
                    workspace_id: id,
                    workspace_name: if name.is_empty() {
                        format!("Tab #{}", tab_id + 1)
                    } else {
                        name.to_owned()
                    },
                },
                native_name: name.to_owned(),
                explicit: false,
            },
        );
    }

    fn bind_layout(&mut self, tab_id: usize, layout: &TiledPaneLayout) {
        if self.closed_tabs.contains(&tab_id) {
            return;
        }
        if !self.tabs.contains_key(&tab_id) {
            self.pending_tabs.insert(tab_id);
        }
        self.ensure_tab(tab_id, "");
        if let (Some(id), Some(name)) = (
            layout.context_metadata.get("bkpr_workspace_id"),
            layout.context_metadata.get("bkpr_workspace_name"),
        ) {
            if !id.is_empty() && !name.is_empty() {
                let canonical_name = self
                    .tabs
                    .iter()
                    .find(|(other_tab, binding)| {
                        **other_tab != tab_id && binding.context.workspace_id == *id
                    })
                    .map(|(_, binding)| binding.context.workspace_name.clone())
                    .unwrap_or_else(|| name.clone());
                let binding = self.tabs.get_mut(&tab_id).unwrap();
                binding.context.workspace_id = id.clone();
                binding.context.workspace_name = canonical_name;
                binding.explicit = true;
            }
        }
    }

    fn prepare_layout(&mut self, tab_id: usize, layout: &TiledPaneLayout) {
        // Screen alone authorizes creating a new tab, including a reused native ID.
        self.closed_tabs.remove(&tab_id);
        self.bind_layout(tab_id, layout);
    }

    fn update(&mut self, plugin_id: u32, token: &str, update: Update) -> bool {
        if self.owner_token.is_empty()
            || token != self.owner_token
            || update.session_id != self.session_id
            || self.owner_plugin != Some(plugin_id)
            || self.owner_lease.is_none()
        {
            return false;
        }
        if let Some((owner, generation)) = self.owner {
            if plugin_id < owner || (plugin_id == owner && update.generation <= generation) {
                return false;
            }
        }
        if update.tabs.iter().any(|tab| {
            tab.workspace_id.is_empty()
                || tab.workspace_name.is_empty()
                || !self.tabs.contains_key(&tab.tab_id)
        }) {
            return false;
        }
        let mut names = BTreeMap::new();
        let mut tabs = BTreeSet::new();
        for tab in &update.tabs {
            if !tabs.insert(tab.tab_id) {
                return false;
            }
            if let Some(name) = names.insert(tab.workspace_id.as_str(), tab.workspace_name.as_str())
            {
                if name != tab.workspace_name {
                    return false;
                }
            }
        }
        for tab in &update.tabs {
            let binding = self.tabs.get_mut(&tab.tab_id).unwrap();
            binding.context.clone_from(tab);
            binding.explicit = true;
        }
        for binding in self.tabs.values_mut() {
            if let Some(name) = names.get(binding.context.workspace_id.as_str()) {
                binding.context.workspace_name = (*name).to_owned();
            }
        }
        self.owner = Some((plugin_id, update.generation));
        true
    }

    fn resolve_target(&self, target: ClientTabIndexOrPaneId) -> Option<usize> {
        match target {
            ClientTabIndexOrPaneId::TabIndex(id) | ClientTabIndexOrPaneId::TabIndexNoFocus(id) => {
                self.tabs.contains_key(&id).then_some(id)
            },
            ClientTabIndexOrPaneId::ClientId(id) | ClientTabIndexOrPaneId::ClientIdNoFocus(id) => {
                self.clients.get(&id).copied()
            },
            ClientTabIndexOrPaneId::PaneId(id) => self.panes.get(&pane_key(id)).copied(),
        }
    }

    fn reserve_terminal(
        &mut self,
        tab_id: usize,
        terminal_id: u32,
    ) -> std::io::Result<TerminalEnvironment> {
        if self.closed_tabs.contains(&tab_id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Bkpr destination tab was closed",
            ));
        }
        let environment = self.terminal_environment(tab_id, terminal_id)?;
        self.bind_terminal(tab_id, terminal_id);
        Ok(environment)
    }

    fn bind_terminal(&mut self, tab_id: usize, terminal_id: u32) {
        let pane = format!("terminal_{}", terminal_id);
        self.pending_panes.insert(pane.clone());
        self.panes.insert(pane, tab_id);
    }

    fn synchronize<'a>(
        &mut self,
        tabs: impl Iterator<Item = (usize, &'a str, Vec<PaneId>)>,
        clients: &BTreeMap<ClientId, usize>,
    ) {
        let mut live_tabs = BTreeSet::new();
        let mut panes = BTreeMap::new();
        for (tab_id, name, pane_ids) in tabs {
            self.ensure_tab(tab_id, name);
            let binding = self.tabs.get_mut(&tab_id).unwrap();
            if binding.native_name != name {
                binding.native_name = name.to_owned();
                if !binding.explicit {
                    binding.context.workspace_name = name.to_owned();
                }
            }
            live_tabs.insert(tab_id);
            for pane_id in pane_ids {
                panes.insert(pane_key(pane_id), tab_id);
            }
        }
        self.pending_tabs.retain(|id| !live_tabs.contains(id));
        let pending_tabs = &self.pending_tabs;
        self.closed_tabs.extend(
            self.tabs
                .keys()
                .filter(|id| !live_tabs.contains(id) && !pending_tabs.contains(id))
                .copied(),
        );
        self.tabs
            .retain(|id, _| live_tabs.contains(id) || pending_tabs.contains(id));
        for pane in panes.keys() {
            self.pending_panes.remove(pane);
        }

        for (pane, tab_id) in &self.panes {
            if self.pending_panes.contains(pane) && self.tabs.contains_key(tab_id) {
                panes.insert(pane.clone(), *tab_id);
            }
        }
        self.panes = panes;
        self.clients.clone_from(clients);
    }

    fn terminal_environment(
        &self,
        tab_id: usize,
        terminal_id: u32,
    ) -> std::io::Result<TerminalEnvironment> {
        let tab = &self
            .tabs
            .get(&tab_id)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Bkpr destination tab was closed",
                )
            })?
            .context;
        Ok(TerminalEnvironment {
            session_id: self.session_id.clone(),
            session_name: self.session_name.clone(),
            workspace_id: tab.workspace_id.clone(),
            workspace_name: tab.workspace_name.clone(),
            pane_id: format!("terminal_{}", terminal_id),
        })
    }

    fn restart_owner(
        &mut self,
        session_id: &str,
        closed_owner: u32,
        source: &RunPluginLocation,
    ) -> Option<RunPluginOrAlias> {
        if self.session_id != session_id
            || self.owner_plugin != Some(closed_owner)
            || self.owner_lease.is_some()
            || self.restart_pending
        {
            return None;
        }
        let configuration = self.owner_configuration.clone()?;
        if configuration.get_run_plugin()?.location != *source {
            return None;
        }
        self.restart_pending = true;
        Some(configuration)
    }

    fn unload_owner(&mut self, plugin_id: u32) {
        self.protected_plugins.remove(&plugin_id);
        if self.owner_plugin == Some(plugin_id) {
            self.owner_lease.take();
        }
    }
    fn snapshot(&self) -> Snapshot<'_> {
        let panes = self
            .panes
            .iter()
            .filter_map(|(pane_id, tab_id)| {
                if self.pending_tabs.contains(tab_id) {
                    return None;
                }
                let tab = &self.tabs.get(tab_id)?.context;
                let root_pid = pane_id
                    .strip_prefix("terminal_")
                    .and_then(|id| id.parse::<u32>().ok())
                    .and_then(|id| self.root_pids.get(&id).copied());
                Some(PaneContext {
                    pane_id: pane_id.clone(),
                    tab_id: *tab_id,
                    workspace_id: tab.workspace_id.clone(),
                    workspace_name: tab.workspace_name.clone(),
                    root_pid,
                })
            })
            .collect();
        Snapshot {
            version: 1,
            session_id: &self.session_id,
            session_name: &self.session_name,
            tabs: self.tabs.values()
                .filter(|tab| !self.pending_tabs.contains(&tab.context.tab_id))
                .map(|tab| &tab.context).collect(),
            panes,
        }
    }

    fn event(&self) -> Event {
        Event::CustomMessage(
            "bkpr_context".into(),
            serde_json::to_string(&self.snapshot()).expect("native context is serializable"),
        )
    }
}

static REGISTRY: LazyLock<Option<Mutex<Registry>>> = LazyLock::new(|| {
    if std::env::var("BKPR_NATIVE_CONTEXT").ok().as_deref() != Some("1") {
        return None;
    }
    let session_id = std::env::var("BKPR_SESSION_ID")
        .ok()
        .filter(|s| !s.is_empty())?;
    let session_name = std::env::var("BKPR_SESSION_NAME")
        .ok()
        .filter(|s| !s.is_empty())?;
    Some(Mutex::new(Registry::new(
        session_id,
        session_name,
        String::new(),
    )))
});

fn registry() -> Option<&'static Mutex<Registry>> {
    REGISTRY.as_ref()
}

/// Same-user control verifies the immutable server incarnation, never an owner token.
pub(crate) fn validate_control_context(expected_session: &str, source_pane: Option<&str>) -> Result<(), String> {
    let registry = registry().ok_or("session has no native Bkpr ownership")?;
    registry.lock().map_err(|_| "native context lock poisoned")?
        .validate_control_context(expected_session, source_pane)
}

pub(crate) fn control_snapshot(
    expected_session: &str,
    source_pane: Option<&str>,
) -> Result<serde_json::Value, String> {
    let registry = registry().ok_or("session has no native Bkpr ownership")?;
    let registry = registry.lock().map_err(|_| "native context lock poisoned")?;
    registry.validate_control_context(expected_session, source_pane)?;
    serde_json::to_value(registry.snapshot()).map_err(|error| error.to_string())
}

pub(crate) fn is_owner_pane(pane_id: PaneId) -> bool {
    let PaneId::Plugin(id) = pane_id else { return false; };
    registry().is_some_and(|registry| {
        registry.lock().is_ok_and(|registry| registry.owner_plugin == Some(id) || registry.protected_plugins.contains(&id))
    })
}

pub(crate) fn owner_plugin_location() -> Option<RunPluginLocation> {
    let registry = registry()?.lock().ok()?;
    Some(registry.owner_configuration.as_ref()?.get_run_plugin()?.location.clone())
}

pub(crate) fn rename_session(name: &str) -> Result<(), String> {
    let registry = registry().ok_or("session has no native Bkpr ownership")?;
    registry.lock().map_err(|_| "native context lock poisoned")?.rename_session(name)
}

pub(crate) fn tab_context(tab_id: usize) -> Option<TabContext> {
    Some(registry()?.lock().ok()?.tabs.get(&tab_id)?.context.clone())
}

pub(crate) fn workspace_tab_ids(workspace_id: &str) -> Result<Vec<usize>, String> {
    let registry = registry().ok_or("session has no native Bkpr ownership")?;
    let registry = registry.lock().map_err(|_| "native context lock poisoned")?;
    let tabs: Vec<_> = registry.tabs.iter()
        .filter_map(|(id, tab)| (tab.context.workspace_id == workspace_id).then_some(*id))
        .collect();
    if tabs.is_empty() { Err("workspace is not live in this session".into()) } else { Ok(tabs) }
}

pub(crate) fn rename_workspace(workspace_id: &str, name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 4096 || name.chars().any(char::is_control) {
        return Err("workspace name must be nonempty printable text of at most 4096 bytes".into());
    }
    let registry = registry().ok_or("session has no native Bkpr ownership")?;
    let mut registry = registry.lock().map_err(|_| "native context lock poisoned")?;
    let mut found = false;
    for binding in registry.tabs.values_mut() {
        if binding.context.workspace_id == workspace_id {
            binding.context.workspace_name = name.into();
            binding.explicit = true;
            found = true;
        }
    }
    if found { Ok(()) } else { Err("workspace is not live in this session".into()) }
}

thread_local! { static SPAWN_TAB: Cell<Option<usize>> = const { Cell::new(None) }; }

/// Scoped destination on the synchronous PTY thread, including direct programs.
pub(crate) struct SpawnScope(Option<usize>);
impl Drop for SpawnScope {
    fn drop(&mut self) {
        SPAWN_TAB.with(|tab| tab.set(self.0));
    }
}

pub(crate) fn spawn_scope(target: ClientTabIndexOrPaneId) -> SpawnScope {
    let tab_id = registry().and_then(|registry| registry.lock().unwrap().resolve_target(target));
    SpawnScope(SPAWN_TAB.with(|tab| tab.replace(tab_id)))
}

pub(crate) fn layout_scope(tab_id: usize, layout: &TiledPaneLayout) -> SpawnScope {
    if let Some(registry) = registry() {
        registry.lock().unwrap().bind_layout(tab_id, layout);
    }
    SpawnScope(SPAWN_TAB.with(|tab| tab.replace(Some(tab_id))))
}

pub(crate) fn prepare_layout(tab_id: usize, layout: &TiledPaneLayout) {
    if let Some(registry) = registry() {
        registry.lock().unwrap().prepare_layout(tab_id, layout);
    }
}

pub(crate) fn reserve_terminal(terminal_id: u32) -> std::io::Result<Option<TerminalEnvironment>> {
    let Some(registry) = registry() else {
        return Ok(None);
    };
    let tab_id = SPAWN_TAB.with(Cell::get).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Bkpr terminal has no live destination",
        )
    })?;
    registry
        .lock()
        .unwrap()
        .reserve_terminal(tab_id, terminal_id)
        .map(Some)
}

pub(crate) fn reserve_held_terminal(terminal_id: u32) -> std::io::Result<()> {
    let Some(registry) = registry() else {
        return Ok(());
    };
    let mut registry = registry.lock().unwrap();
    let tab_id = SPAWN_TAB
        .with(Cell::get)
        .filter(|tab_id| {
            registry.tabs.contains_key(tab_id) && !registry.closed_tabs.contains(tab_id)
        })
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Bkpr terminal has no live destination",
            )
        })?;
    registry.bind_terminal(tab_id, terminal_id);
    Ok(())
}

pub(crate) fn rerun_environment(terminal_id: u32) -> std::io::Result<Option<TerminalEnvironment>> {
    let Some(registry) = registry() else {
        return Ok(None);
    };
    let registry = registry.lock().unwrap();
    let tab_id = registry
        .panes
        .get(&format!("terminal_{}", terminal_id))
        .copied()
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "Bkpr terminal was closed")
        })?;
    registry.terminal_environment(tab_id, terminal_id).map(Some)
}

pub(crate) fn set_root_pid(terminal_id: u32, pid: u32) {
    if let Some(registry) = registry() {
        registry.lock().unwrap().root_pids.insert(terminal_id, pid);
    }
}

pub(crate) fn close_terminal(terminal_id: u32) {
    if let Some(registry) = registry() {
        let mut registry = registry.lock().unwrap();
        registry.root_pids.remove(&terminal_id);
        registry.panes.remove(&format!("terminal_{}", terminal_id));
        registry
            .pending_panes
            .remove(&format!("terminal_{}", terminal_id));
    }
}

pub(crate) fn ensure_tab(tab_id: usize, name: &str) {
    if let Some(registry) = registry() {
        let mut registry = registry.lock().unwrap();
        registry.ensure_tab(tab_id, name);
    }
}

pub(crate) fn discard_pending_tab(tab_id: usize) {
    let Some(registry) = registry() else {
        return;
    };
    let mut registry = registry.lock().unwrap();
    if !registry.pending_tabs.remove(&tab_id) {
        return;
    }
    registry.closed_tabs.insert(tab_id);
    registry.tabs.remove(&tab_id);
    let Registry {
        panes,
        pending_panes,
        root_pids,
        ..
    } = &mut *registry;
    panes.retain(|pane, bound_tab| {
        if *bound_tab != tab_id {
            return true;
        }
        pending_panes.remove(pane);
        if let Some(id) = pane
            .strip_prefix("terminal_")
            .and_then(|id| id.parse::<u32>().ok())
        {
            root_pids.remove(&id);
        }
        false
    });
}

fn pane_key(pane_id: PaneId) -> String {
    match pane_id {
        PaneId::Terminal(id) => format!("terminal_{}", id),
        PaneId::Plugin(id) => format!("plugin_{}", id),
    }
}

/// Called by Screen before publishing topology or dispatching another action.
pub(crate) fn synchronize<'a>(
    tabs: impl Iterator<Item = (usize, &'a str, Vec<PaneId>)>,
    clients: &BTreeMap<ClientId, usize>,
) {
    let Some(registry) = registry() else {
        return;
    };
    registry.lock().unwrap().synchronize(tabs, clients);
}

pub(crate) fn set_clients(clients: &BTreeMap<ClientId, usize>) {
    if let Some(registry) = registry() {
        let mut registry = registry.lock().unwrap();
        if registry.clients != *clients {
            registry.clients.clone_from(clients);
        }
    }
}

pub(crate) fn event() -> Option<Event> {
    registry().map(|r| r.lock().unwrap().event())
}

pub(crate) fn update(plugin_id: u32, token: &str, payload: &str) -> bool {
    let Some(registry) = registry() else {
        return false;
    };
    let Ok(update) = serde_json::from_str(payload) else {
        return false;
    };
    registry.lock().unwrap().update(plugin_id, token, update)
}

/// Only the session's original load_plugins configuration may establish authority.
pub(crate) fn register_startup_owner(plugin: &RunPluginOrAlias) {
    let Some(registry) = registry() else {
        return;
    };
    let Some(run) = plugin.get_run_plugin() else {
        return;
    };
    let config = run.configuration.inner();
    let mut registry = registry.lock().unwrap();
    if registry.owner_configuration.is_none()
        && config.get("role").map(String::as_str) == Some("native-owner")
        && config.get("session_id") == Some(&registry.session_id)
    {
        if let Some(token) = config
            .get("context_owner")
            .filter(|token| !token.is_empty())
        {
            registry.owner_token.clone_from(token);
            registry.owner_configuration = Some(plugin.clone());
        }
    }
}

pub(crate) fn owner_loaded(plugin: &zellij_utils::input::layout::RunPlugin, plugin_id: u32) {
    let Some(registry) = registry() else {
        return;
    };
    let mut registry = registry.lock().unwrap();
    if registry.owner_configuration.as_ref()
        .and_then(RunPluginOrAlias::get_run_plugin)
        .is_some_and(|owner| owner.location == plugin.location)
    {
        registry.protected_plugins.insert(plugin_id);
    }
    if registry
        .owner_configuration
        .as_ref()
        .and_then(RunPluginOrAlias::get_run_plugin)
        .as_ref()
        == Some(plugin)
    {
        registry.owner_lease.take();
        registry.owner_plugin = None;
        match acquire_owner_lease(&registry.session_id, plugin_id) {
            Ok(lease) => {
                registry.owner_lease = Some(lease);
                registry.owner_plugin = Some(plugin_id);
            },
            Err(error) => log::error!("Failed to acquire Bkpr native owner lease: {}", error),
        }
        registry.restart_pending = false;
    }
}

pub(crate) fn owner_unloaded(plugin_id: u32) {
    if let Some(registry) = registry() {
        registry.lock().unwrap().unload_owner(plugin_id);
    }
}

pub(crate) fn restart_owner(
    session_id: &str,
    closed_owner: u32,
    source: &RunPluginLocation,
) -> Option<RunPluginOrAlias> {
    registry()?
        .lock()
        .unwrap()
        .restart_owner(session_id, closed_owner, source)
}

pub(crate) fn owner_load_failed(plugin: &RunPluginOrAlias) {
    if let Some(registry) = registry() {
        let mut registry = registry.lock().unwrap();
        if registry.owner_configuration.as_ref() == Some(plugin) {
            registry.restart_pending = false;
        }
    }
}

/// Remove inherited Bkpr authority at every unmanaged/background child boundary.
pub(crate) fn clear_environment(command: &mut std::process::Command) -> &mut std::process::Command {
    for key in CONTEXT_ENV {
        command.env_remove(key);
    }
    command
        .env_remove("BKPR_CONTEXT_OWNER")
        .env_remove("BKPR_NATIVE_CONTEXT")
}

/// Managed terminals replace the cleared namespace with their destination context.
pub(crate) fn apply_environment(
    command: &mut std::process::Command,
    environment: Option<&TerminalEnvironment>,
) {
    match environment {
        Some(environment) => environment.apply(command),
        None => {
            clear_environment(command);
        },
    }
}

fn acquire_owner_lease(session_id: &str, plugin_id: u32) -> std::io::Result<std::fs::File> {
    use std::{
        io,
        path::{Component, Path, PathBuf},
    };
    let mut components = Path::new(session_id).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Bkpr session identity",
        ));
    }
    let nonempty_env = |name| std::env::var_os(name).filter(|value| !value.is_empty());
    let root = if let Some(runtime) = nonempty_env("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime).join("bkpr")
    } else if let Some(state) = nonempty_env("XDG_STATE_HOME") {
        PathBuf::from(state).join("bkpr/run")
    } else {
        PathBuf::from(nonempty_env("HOME").ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Bkpr context directory is unavailable",
            )
        })?)
        .join(".local/state/bkpr/run")
    };
    if !root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Bkpr context directory must be absolute",
        ));
    }
    acquire_owner_lease_in(&root.join("contexts").join(session_id), plugin_id)
}

#[cfg(unix)]
fn acquire_owner_lease_in(
    directory: &std::path::Path,
    plugin_id: u32,
) -> std::io::Result<std::fs::File> {
    use std::{
        ffi::CString,
        fs::{File, OpenOptions, TryLockError},
        io,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::fs::{MetadataExt, OpenOptionsExt},
        },
    };
    // Bootstrap owns directory creation. Never create or chmod a user directory here.
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory)?;
    let metadata = directory.metadata()?;
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o777 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe Bkpr context directory",
        ));
    }
    let name = CString::new(format!("owner-{plugin_id}.lock")).unwrap();
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let lease = unsafe { File::from_raw_fd(fd) };
    let metadata = lease.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe Bkpr owner lease",
        ));
    }
    lease.try_lock().map_err(|error| match error {
        TryLockError::WouldBlock => {
            io::Error::new(io::ErrorKind::WouldBlock, "Bkpr owner lease already held")
        },
        TryLockError::Error(error) => error,
    })?;
    Ok(lease)
}

#[cfg(not(unix))]
fn acquire_owner_lease_in(
    _directory: &std::path::Path,
    _plugin_id: u32,
) -> std::io::Result<std::fs::File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Bkpr native owner leases require Unix",
    ))
}
#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Registry {
        let mut registry = Registry::new("incarnation-a".into(), "work".into(), "secret".into());
        registry.owner_plugin = Some(2);
        let lease = tempfile::tempfile().unwrap();
        lease.try_lock().unwrap();
        registry.owner_lease = Some(lease);
        registry
    }

    #[test]
    fn display_rename_preserves_incarnation_authority_and_old_process_snapshot() {
        let mut registry = registry();
        registry.ensure_tab(0, "workspace");
        let before = registry.terminal_environment(0, 7).unwrap();
        registry.rename_session("new display label").unwrap();
        let after = registry.terminal_environment(0, 8).unwrap();
        assert_eq!(before.session_name, "work");
        assert_eq!(after.session_name, "new display label");
        assert_eq!(before.session_id, after.session_id);
        assert_eq!(registry.owner_token, "secret");
        assert_eq!(registry.owner_plugin, Some(2));
        assert!(registry.owner_lease.is_some());
        assert!(registry.rename_session("bad\nlabel").is_err());
        assert_eq!(registry.session_name, "new display label");
    }

    #[test]
    fn control_rejects_reused_session_names_pending_and_closed_sources() {
        let mut registry = registry();
        registry.synchronize(vec![(0, "one", vec![PaneId::Terminal(7)])].into_iter(), &BTreeMap::new());
        assert!(registry.validate_control_context("incarnation-a", Some("terminal_7")).is_ok());
        assert!(registry.validate_control_context("incarnation-b", Some("terminal_7")).is_err());
        assert!(registry.validate_control_context("incarnation-a", Some("7")).is_err());
        registry.pending_panes.insert("terminal_7".into());
        assert!(registry.validate_control_context("incarnation-a", Some("terminal_7")).is_err());
        registry.pending_panes.clear();
        registry.synchronize(vec![(0, "one", vec![])].into_iter(), &BTreeMap::new());
        assert!(registry.validate_control_context("incarnation-a", Some("terminal_7")).is_err());
    }

    #[test]
    fn stock_new_tabs_are_distinct_and_reused_session_names_do_not_reuse_ids() {
        let mut first = registry();
        first.ensure_tab(0, "one");
        first.ensure_tab(1, "two");
        assert_ne!(
            first.tabs[&0].context.workspace_id,
            first.tabs[&1].context.workspace_id
        );
        let mut next = Registry::new("incarnation-b".into(), "work".into(), "secret".into());
        next.ensure_tab(0, "one");
        assert_ne!(
            first.tabs[&0].context.workspace_id,
            next.tabs[&0].context.workspace_id
        );
    }

    #[test]
    fn explicit_grouping_preserves_workspace_name_across_native_tab_names() {
        let mut registry = registry();
        let mut layout = TiledPaneLayout::default();
        layout
            .context_metadata
            .insert("bkpr_workspace_id".into(), "group".into());
        layout
            .context_metadata
            .insert("bkpr_workspace_name".into(), "workspace".into());
        registry.bind_layout(0, &layout);
        registry.bind_layout(1, &layout);
        assert_eq!(
            registry.tabs[&0].context.workspace_id,
            registry.tabs[&1].context.workspace_id
        );
        assert_eq!(registry.tabs[&0].context.workspace_name, "workspace");
        registry.synchronize(
            vec![(0, "native one", vec![]), (1, "native two", vec![])].into_iter(),
            &BTreeMap::new(),
        );
        assert_eq!(registry.tabs[&1].context.workspace_name, "workspace");
        assert!(registry.update(
            2,
            "secret",
            Update {
                session_id: "incarnation-a".into(),
                generation: 1,
                tabs: vec![TabContext {
                    tab_id: 0,
                    workspace_id: "group".into(),
                    workspace_name: "renamed group".into()
                }],
            }
        ));
        assert_eq!(registry.tabs[&1].context.workspace_name, "renamed group");
    }

    #[test]
    fn updates_reject_foreign_sessions_tokens_owners_and_stale_generations() {
        let mut registry = registry();
        registry.ensure_tab(0, "one");
        let update = |session: &str, generation| Update {
            session_id: session.into(),
            generation,
            tabs: vec![TabContext {
                tab_id: 0,
                workspace_id: "group".into(),
                workspace_name: "renamed".into(),
            }],
        };
        assert!(!registry.update(2, "wrong", update("incarnation-a", 1)));
        assert!(!registry.update(2, "secret", update("old-session", 1)));
        assert!(registry.update(2, "secret", update("incarnation-a", 1)));
        assert!(!registry.update(2, "secret", update("incarnation-a", 1)));
        registry.owner_plugin = Some(3);
        assert!(registry.update(3, "secret", update("incarnation-a", 1)));
        assert!(!registry.update(2, "secret", update("incarnation-a", 99)));
        assert_eq!(registry.tabs[&0].context.workspace_name, "renamed");
    }

    fn environment(command: &std::process::Command) -> BTreeMap<String, Option<String>> {
        command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    #[test]
    fn direct_program_and_stock_split_receive_all_five_values_without_a_shell() {
        let mut registry = registry();
        registry.ensure_tab(0, "Workspace");
        registry.clients.insert(7, 0);
        let first = registry.reserve_terminal(0, 10).unwrap();
        let source = registry
            .resolve_target(ClientTabIndexOrPaneId::ClientId(7))
            .unwrap();
        let split = registry.reserve_terminal(source, 11).unwrap();
        for (id, context) in [(10, first), (11, split)] {
            let mut command = std::process::Command::new("/usr/bin/env");
            for key in CONTEXT_ENV {
                command.env(key, "stale-parent");
            }
            command.env("BKPR_CONTEXT_OWNER", "secret");
            context.apply(&mut command);
            let env = environment(&command);
            assert_eq!(env["BKPR_SESSION_ID"].as_deref(), Some("incarnation-a"));
            assert_eq!(env["BKPR_SESSION_NAME"].as_deref(), Some("work"));
            assert_eq!(
                env["BKPR_WORKSPACE_ID"].as_deref(),
                Some("incarnation-a:workspace:0")
            );
            assert_eq!(env["BKPR_WORKSPACE_NAME"].as_deref(), Some("Workspace"));
            assert_eq!(
                env["BKPR_PANE_ID"].as_deref(),
                Some(format!("terminal_{}", id).as_str())
            );
            assert_eq!(env["BKPR_CONTEXT_OWNER"], None);
        }
    }

    #[test]
    fn move_rename_close_and_future_children_follow_live_topology_not_old_environment() {
        let mut registry = registry();
        registry.ensure_tab(0, "one");
        registry.ensure_tab(1, "two");
        let original_context = registry.reserve_terminal(0, 10).unwrap();
        let mut original = std::process::Command::new("/usr/bin/env");
        original_context.apply(&mut original);
        registry.synchronize(
            vec![
                (0, "one", vec![]),
                (1, "renamed", vec![PaneId::Terminal(10)]),
            ]
            .into_iter(),
            &BTreeMap::new(),
        );
        let destination = registry
            .resolve_target(ClientTabIndexOrPaneId::PaneId(PaneId::Terminal(10)))
            .unwrap();
        let child_context = registry.reserve_terminal(destination, 11).unwrap();
        let mut child = std::process::Command::new("/usr/bin/env");
        child_context.apply(&mut child);
        assert_eq!(
            environment(&original)["BKPR_WORKSPACE_NAME"].as_deref(),
            Some("one")
        );
        assert_eq!(
            environment(&child)["BKPR_WORKSPACE_NAME"].as_deref(),
            Some("renamed")
        );
        assert_eq!(
            environment(&child)["BKPR_WORKSPACE_ID"].as_deref(),
            Some("incarnation-a:workspace:1")
        );
        registry.synchronize(vec![(1, "renamed", vec![])].into_iter(), &BTreeMap::new());
        assert!(!registry.panes.contains_key("terminal_10"));
        assert!(!registry.tabs.contains_key(&0));
    }

    #[test]
    fn recovery_reuses_trusted_configuration_once_and_rejects_foreign_or_live_owners() {
        let mut registry = registry();
        let source = RunPluginLocation::File(std::path::PathBuf::from("/tmp/bkpr.wasm"));
        let template = RunPluginOrAlias::RunPlugin(zellij_utils::input::layout::RunPlugin {
            location: source.clone(),
            ..Default::default()
        });
        registry.owner_configuration = Some(template.clone());
        assert!(registry
            .restart_owner("incarnation-a", 2, &source)
            .is_none());
        registry.owner_lease.take();
        assert!(registry.restart_owner("foreign", 2, &source).is_none());
        assert!(registry
            .restart_owner("incarnation-a", 1, &source)
            .is_none());
        let foreign = RunPluginLocation::File(std::path::PathBuf::from("/tmp/foreign.wasm"));
        assert!(registry
            .restart_owner("incarnation-a", 2, &foreign)
            .is_none());
        assert_eq!(
            registry.restart_owner("incarnation-a", 2, &source),
            Some(template)
        );
        assert!(registry
            .restart_owner("incarnation-a", 2, &source)
            .is_none());
        registry.owner_plugin = Some(3);
        let lease = tempfile::tempfile().unwrap();
        lease.try_lock().unwrap();
        registry.owner_lease = Some(lease);
        registry.restart_pending = false;
        assert!(registry
            .restart_owner("incarnation-a", 2, &source)
            .is_none());
    }

    #[test]
    fn native_snapshot_carries_live_membership_and_root_pid_without_authority_secrets() {
        let mut registry = registry();
        registry.ensure_tab(0, "Workspace");
        registry.reserve_terminal(0, 10).unwrap();
        registry.root_pids.insert(10, 1234);
        let Event::CustomMessage(name, payload) = registry.event() else {
            panic!("expected context");
        };
        assert_eq!(name, "bkpr_context");
        let snapshot: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(snapshot["session_id"], "incarnation-a");
        assert_eq!(snapshot["panes"][0]["pane_id"], "terminal_10");
        assert_eq!(snapshot["panes"][0]["root_pid"], 1234);
        assert_eq!(snapshot["panes"][0]["workspace_name"], "Workspace");
        assert!(!payload.contains("secret"));
        assert!(!payload.contains("context_owner"));
    }

    #[test]
    fn override_tab_context_survives_screen_updates_before_and_during_spawn() {
        let mut registry = registry();
        registry.ensure_tab(0, "existing");
        let mut layout = TiledPaneLayout::default();
        layout
            .context_metadata
            .insert("bkpr_workspace_id".into(), "group".into());
        layout
            .context_metadata
            .insert("bkpr_workspace_name".into(), "Grouped workspace".into());
        registry.bind_layout(9, &layout);
        // Screen still contains only the original tab while PTY prepares the command.
        registry.synchronize(vec![(0, "existing", vec![])].into_iter(), &BTreeMap::new());
        let context = registry.reserve_terminal(9, 40).unwrap();
        // Another Screen event can arrive between reservation and actual exec.
        registry.synchronize(vec![(0, "existing", vec![])].into_iter(), &BTreeMap::new());
        let mut command = std::process::Command::new("/usr/bin/env");
        context.apply(&mut command);
        assert_eq!(
            environment(&command)["BKPR_WORKSPACE_ID"].as_deref(),
            Some("group")
        );
        assert_eq!(
            environment(&command)["BKPR_WORKSPACE_NAME"].as_deref(),
            Some("Grouped workspace")
        );
        let Event::CustomMessage(_, payload) = registry.event() else {
            panic!("expected context");
        };
        let snapshot: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(snapshot["tabs"].as_array().unwrap().len(), 1);
        assert!(snapshot["panes"].as_array().unwrap().is_empty());
        registry.synchronize(
            vec![
                (0, "existing", vec![]),
                (9, "new", vec![PaneId::Terminal(40)]),
            ]
            .into_iter(),
            &BTreeMap::new(),
        );
        assert!(!registry.pending_tabs.contains(&9));
        assert_eq!(registry.panes["terminal_40"], 9);
        // Once installed, ordinary close reconciliation must remove the tab.
        registry.synchronize(vec![(0, "existing", vec![])].into_iter(), &BTreeMap::new());
        assert!(!registry.tabs.contains_key(&9));
        assert!(!registry.panes.contains_key("terminal_40"));
    }

    #[test]
    fn close_between_reservation_and_exec_preserves_context_without_poisoning_registry() {
        let registry = Mutex::new(registry());
        let context = {
            let mut registry = registry.lock().unwrap();
            registry.ensure_tab(0, "creation target");
            registry.ensure_tab(1, "other workspace");
            registry.reserve_terminal(0, 80).unwrap()
        };
        registry.lock().unwrap().synchronize(
            vec![(1, "other workspace", vec![])].into_iter(),
            &BTreeMap::new(),
        );
        let mut command = std::process::Command::new("/usr/bin/env");
        context.apply(&mut command);
        assert_eq!(
            environment(&command)["BKPR_WORKSPACE_NAME"].as_deref(),
            Some("creation target")
        );
        assert_eq!(
            environment(&command)["BKPR_WORKSPACE_ID"].as_deref(),
            Some("incarnation-a:workspace:0")
        );
        let mut registry = registry.lock().unwrap();
        assert!(!registry.tabs.contains_key(&0));
        assert!(!registry.panes.contains_key("terminal_80"));
        assert!(registry.reserve_terminal(0, 81).is_err());
        // A delayed layout override is not a pending creation of an already closed tab.
        registry.bind_layout(0, &TiledPaneLayout::default());
        assert!(!registry.pending_tabs.contains(&0));
        assert!(registry.reserve_terminal(0, 82).is_err());
        assert!(registry.reserve_terminal(99, 83).is_err());
        assert_eq!(
            registry.resolve_target(ClientTabIndexOrPaneId::TabIndex(0)),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn owner_lease_is_exclusive_cloexec_and_released_synchronously_on_unload_and_drop() {
        use std::{
            fs::{OpenOptions, Permissions, TryLockError},
            os::{fd::AsRawFd, unix::fs::PermissionsExt},
        };
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), Permissions::from_mode(0o700)).unwrap();
        let mut registry = registry();
        registry.owner_lease = Some(acquire_owner_lease_in(directory.path(), 2).unwrap());
        let observer = OpenOptions::new()
            .read(true)
            .write(true)
            .open(directory.path().join("owner-2.lock"))
            .unwrap();
        assert!(matches!(observer.try_lock(), Err(TryLockError::WouldBlock)));
        assert!(acquire_owner_lease_in(directory.path(), 2).is_err());
        let flags = unsafe {
            libc::fcntl(
                registry.owner_lease.as_ref().unwrap().as_raw_fd(),
                libc::F_GETFD,
            )
        };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        registry.unload_owner(1);
        assert!(matches!(observer.try_lock(), Err(TryLockError::WouldBlock)));
        registry.unload_owner(2);
        observer.try_lock().unwrap();
        observer.unlock().unwrap();
        assert!(!registry.update(
            2,
            "secret",
            Update {
                session_id: "incarnation-a".into(),
                generation: 1,
                tabs: vec![],
            }
        ));
        // Replacement uses a distinct file; an old owner's unload cannot release it.
        registry.owner_lease = Some(acquire_owner_lease_in(directory.path(), 3).unwrap());
        registry.owner_plugin = Some(3);
        let replacement = OpenOptions::new()
            .read(true)
            .write(true)
            .open(directory.path().join("owner-3.lock"))
            .unwrap();
        registry.unload_owner(2);
        assert!(matches!(
            replacement.try_lock(),
            Err(TryLockError::WouldBlock)
        ));
        drop(registry);
        replacement.try_lock().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn owner_lease_rejects_unsafe_paths_without_creating_context_directories() {
        use std::{
            fs,
            os::unix::fs::{symlink, PermissionsExt},
        };
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let missing = directory.path().join("missing");
        assert!(acquire_owner_lease_in(&missing, 1).is_err());
        assert!(!missing.exists());
        let target = directory.path().join("target");
        fs::write(&target, b"unchanged").unwrap();
        symlink(&target, directory.path().join("owner-1.lock")).unwrap();
        assert!(acquire_owner_lease_in(directory.path(), 1).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"unchanged");
        fs::write(directory.path().join("owner-2.lock"), b"existing").unwrap();
        fs::set_permissions(
            directory.path().join("owner-2.lock"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(acquire_owner_lease_in(directory.path(), 2).is_err());
        let hardlink = directory.path().join("owner-5.lock");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        fs::hard_link(&target, &hardlink).unwrap();
        assert!(acquire_owner_lease_in(directory.path(), 5).is_err());
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(acquire_owner_lease_in(directory.path(), 3).is_err());
        assert!(!directory.path().join("owner-3.lock").exists());
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let alias = directory.path().join("alias");
        symlink(directory.path(), &alias).unwrap();
        assert!(acquire_owner_lease_in(&alias, 4).is_err());
    }

    #[test]
    fn reused_highest_native_tab_gets_a_fresh_stock_workspace_identity() {
        let mut registry = registry();
        registry.ensure_tab(0, "first");
        registry.ensure_tab(1, "highest");
        let closed_id = registry.tabs[&1].context.workspace_id.clone();
        let next = registry.next_workspace_id;
        registry.ensure_tab(1, "still highest");
        assert_eq!(registry.next_workspace_id, next);
        registry.synchronize(vec![(0, "first", vec![])].into_iter(), &BTreeMap::new());
        assert!(registry.closed_tabs.contains(&1));
        registry.ensure_tab(1, "recreated");
        assert_ne!(registry.tabs[&1].context.workspace_id, closed_id);
        assert!(!registry.closed_tabs.contains(&1));
        assert!(registry.reserve_terminal(1, 90).is_ok());
        registry.synchronize(vec![(0, "first", vec![])].into_iter(), &BTreeMap::new());
        // A genuine OverrideLayout creation can also reuse the closed native ID.
        let mut layout = TiledPaneLayout::default();
        layout
            .context_metadata
            .insert("bkpr_workspace_id".into(), "explicit-group".into());
        layout
            .context_metadata
            .insert("bkpr_workspace_name".into(), "Explicit group".into());
        registry.prepare_layout(1, &layout);
        registry.synchronize(vec![(0, "first", vec![])].into_iter(), &BTreeMap::new());
        let context = registry.reserve_terminal(1, 91).unwrap();
        assert_eq!(context.workspace_id, "explicit-group");
        assert!(registry.pending_tabs.contains(&1));
    }
}

#[cfg(test)]
mod environment_boundary_tests {
    use super::*;

    #[test]
    fn unmanaged_nested_runtime_clears_context_without_touching_other_environment() {
        let mut command = std::process::Command::new("/usr/bin/env");
        for key in CONTEXT_ENV {
            command.env(key, "outer-session");
        }
        command
            .env("BKPR_NATIVE_CONTEXT", "1")
            .env("BKPR_CONTEXT_OWNER", "secret");
        command.env("UNRELATED", "preserved");
        clear_environment(&mut command);
        for (key, value) in command.get_envs() {
            if key == "UNRELATED" {
                assert_eq!(value, Some(std::ffi::OsStr::new("preserved")));
            } else {
                assert!(value.is_none(), "inherited Bkpr namespace leaked");
            }
        }
    }
}
