//! Bkpr's creation-time context and live native topology. No plugin round trip
//! is involved in choosing the environment of a terminal process.
use crate::{panes::PaneId, pty::ClientTabIndexOrPaneId, ClientId};
use serde::{Deserialize, Serialize};
use std::{cell::Cell, collections::{BTreeMap, BTreeSet}, sync::{LazyLock, Mutex}};
use zellij_utils::{data::Event, input::layout::{RunPluginLocation, RunPluginOrAlias, TiledPaneLayout}};

const CONTEXT_ENV: [&str; 5] = [
    "BKPR_SESSION_ID", "BKPR_SESSION_NAME", "BKPR_WORKSPACE_ID",
    "BKPR_WORKSPACE_NAME", "BKPR_PANE_ID",
];

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
    owner_alive: bool,
    restart_pending: bool,
    owner_configuration: Option<RunPluginOrAlias>,
    tabs: BTreeMap<usize, TabBinding>,
    panes: BTreeMap<String, usize>,
    pending_panes: BTreeSet<String>,
    root_pids: BTreeMap<u32, u32>,
    clients: BTreeMap<ClientId, usize>,
}

impl Registry {
    fn new(session_id: String, session_name: String, owner_token: String) -> Self {
        Self { session_id, session_name, owner_token, owner: None,
            owner_plugin: None, owner_alive: false, restart_pending: false,
            owner_configuration: None, tabs: BTreeMap::new(),
            panes: BTreeMap::new(), pending_panes: BTreeSet::new(),
            root_pids: BTreeMap::new(), clients: BTreeMap::new() }
    }

    fn ensure_tab(&mut self, tab_id: usize, name: &str) {
        let id = format!("{}:workspace:{}", self.session_id, tab_id);
        self.tabs.entry(tab_id).or_insert_with(|| TabBinding {
            context: TabContext { tab_id, workspace_id: id,
                workspace_name: if name.is_empty() { format!("Tab #{}", tab_id + 1) } else { name.to_owned() } },
            native_name: name.to_owned(), explicit: false,
        });
    }

    fn bind_layout(&mut self, tab_id: usize, layout: &TiledPaneLayout) {
        self.ensure_tab(tab_id, "");
        if let (Some(id), Some(name)) = (layout.context_metadata.get("bkpr_workspace_id"),
            layout.context_metadata.get("bkpr_workspace_name")) {
            if !id.is_empty() && !name.is_empty() {
                let canonical_name = self.tabs.iter()
                    .find(|(other_tab, binding)| **other_tab != tab_id && binding.context.workspace_id == *id)
                    .map(|(_, binding)| binding.context.workspace_name.clone())
                    .unwrap_or_else(|| name.clone());
                let binding = self.tabs.get_mut(&tab_id).unwrap();
                binding.context.workspace_id = id.clone();
                binding.context.workspace_name = canonical_name;
                binding.explicit = true;
            }
        }
    }

    fn update(&mut self, plugin_id: u32, token: &str, update: Update) -> bool {
        if self.owner_token.is_empty() || token != self.owner_token || update.session_id != self.session_id
            || self.owner_plugin != Some(plugin_id) || !self.owner_alive {
            return false;
        }
        if let Some((owner, generation)) = self.owner {
            if plugin_id < owner || (plugin_id == owner && update.generation <= generation) { return false; }
        }
        if update.tabs.iter().any(|tab| tab.workspace_id.is_empty() || tab.workspace_name.is_empty()
            || !self.tabs.contains_key(&tab.tab_id)) { return false; }
        let mut names = BTreeMap::new();
        let mut tabs = BTreeSet::new();
        for tab in &update.tabs {
            if !tabs.insert(tab.tab_id) { return false; }
            if let Some(name) = names.insert(tab.workspace_id.as_str(), tab.workspace_name.as_str()) {
                if name != tab.workspace_name { return false; }
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
            ClientTabIndexOrPaneId::TabIndex(id) | ClientTabIndexOrPaneId::TabIndexNoFocus(id) => Some(id),
            ClientTabIndexOrPaneId::ClientId(id) | ClientTabIndexOrPaneId::ClientIdNoFocus(id) => self.clients.get(&id).copied(),
            ClientTabIndexOrPaneId::PaneId(id) => self.panes.get(&pane_key(id)).copied(),
        }
    }

    fn reserve_terminal(&mut self, tab_id: usize, terminal_id: u32) {
        self.ensure_tab(tab_id, "");
        let pane = format!("terminal_{}", terminal_id);
        self.pending_panes.insert(pane.clone());
        self.panes.insert(pane, tab_id);
    }

    fn synchronize<'a>(&mut self, tabs: impl Iterator<Item = (usize, &'a str, Vec<PaneId>)>, clients: &BTreeMap<ClientId, usize>) {
        let mut live_tabs = BTreeSet::new();
        let mut panes = BTreeMap::new();
        for (tab_id, name, pane_ids) in tabs {
            self.ensure_tab(tab_id, name);
            let binding = self.tabs.get_mut(&tab_id).unwrap();
            if binding.native_name != name {
                binding.native_name = name.to_owned();
                if !binding.explicit { binding.context.workspace_name = name.to_owned(); }
            }
            live_tabs.insert(tab_id);
            for pane_id in pane_ids { panes.insert(pane_key(pane_id), tab_id); }
        }
        self.tabs.retain(|id, _| live_tabs.contains(id));
        for pane in panes.keys() { self.pending_panes.remove(pane); }
        for (pane, tab_id) in &self.panes {
            if self.pending_panes.contains(pane) && live_tabs.contains(tab_id) {
                panes.insert(pane.clone(), *tab_id);
            }
        }
        self.panes = panes;
        self.clients.clone_from(clients);
    }

    fn apply_environment(&self, command: &mut std::process::Command, terminal_id: u32) {
        let pane_id = format!("terminal_{}", terminal_id);
        let tab_id = self.panes.get(&pane_id).expect("Bkpr terminal must have a destination tab before exec");
        let tab = &self.tabs.get(tab_id).expect("Bkpr destination tab must exist before exec").context;
        clear_environment(command);
        command.env("BKPR_SESSION_ID", &self.session_id)
            .env("BKPR_SESSION_NAME", &self.session_name)
            .env("BKPR_WORKSPACE_ID", &tab.workspace_id)
            .env("BKPR_WORKSPACE_NAME", &tab.workspace_name)
            .env("BKPR_PANE_ID", pane_id);
    }

    fn restart_owner(&mut self, session_id: &str, closed_owner: u32, source: &RunPluginLocation) -> Option<RunPluginOrAlias> {
        if self.session_id != session_id || self.owner_plugin != Some(closed_owner)
            || self.owner_alive || self.restart_pending { return None; }
        let configuration = self.owner_configuration.clone()?;
        if configuration.get_run_plugin()?.location != *source { return None; }
        self.restart_pending = true;
        Some(configuration)
    }

    fn event(&self) -> Event {
        let panes = self.panes.iter().filter_map(|(pane_id, tab_id)| {
            let tab = &self.tabs.get(tab_id)?.context;
            let root_pid = pane_id.strip_prefix("terminal_").and_then(|id| id.parse::<u32>().ok())
                .and_then(|id| self.root_pids.get(&id).copied());
            Some(PaneContext { pane_id: pane_id.clone(), tab_id: *tab_id,
                workspace_id: tab.workspace_id.clone(), workspace_name: tab.workspace_name.clone(), root_pid })
        }).collect();
        Event::CustomMessage("bkpr_context".into(), serde_json::to_string(&Snapshot {
            version: 1, session_id: &self.session_id, session_name: &self.session_name,
            tabs: self.tabs.values().map(|tab| &tab.context).collect(), panes,
        }).expect("native context is serializable"))
    }
}

static REGISTRY: LazyLock<Option<Mutex<Registry>>> = LazyLock::new(|| {
    if std::env::var("BKPR_NATIVE_CONTEXT").ok().as_deref() != Some("1") { return None; }
    let session_id = std::env::var("BKPR_SESSION_ID").ok().filter(|s| !s.is_empty())?;
    let session_name = std::env::var("BKPR_SESSION_NAME").ok().filter(|s| !s.is_empty())?;
    Some(Mutex::new(Registry::new(session_id, session_name, String::new())))
});

fn registry() -> Option<&'static Mutex<Registry>> { REGISTRY.as_ref() }

thread_local! { static SPAWN_TAB: Cell<Option<usize>> = const { Cell::new(None) }; }

/// Scoped destination on the synchronous PTY thread, including direct programs.
pub(crate) struct SpawnScope(Option<usize>);
impl Drop for SpawnScope {
    fn drop(&mut self) { SPAWN_TAB.with(|tab| tab.set(self.0)); }
}

pub(crate) fn spawn_scope(target: ClientTabIndexOrPaneId) -> SpawnScope {
    let tab_id = registry().and_then(|registry| registry.lock().unwrap().resolve_target(target));
    SpawnScope(SPAWN_TAB.with(|tab| tab.replace(tab_id)))
}

pub(crate) fn layout_scope(tab_id: usize, layout: &TiledPaneLayout) -> SpawnScope {
    if let Some(registry) = registry() { registry.lock().unwrap().bind_layout(tab_id, layout); }
    SpawnScope(SPAWN_TAB.with(|tab| tab.replace(Some(tab_id))))
}

pub(crate) fn reserve_terminal(terminal_id: u32) {
    if let Some(registry) = registry() {
        let mut registry = registry.lock().unwrap();
        if let Some(tab_id) = SPAWN_TAB.with(Cell::get) {
            registry.reserve_terminal(tab_id, terminal_id);
        }
    }
}

pub(crate) fn set_root_pid(terminal_id: u32, pid: u32) {
    if let Some(registry) = registry() { registry.lock().unwrap().root_pids.insert(terminal_id, pid); }
}

pub(crate) fn close_terminal(terminal_id: u32) {
    if let Some(registry) = registry() {
        let mut registry = registry.lock().unwrap();
        registry.root_pids.remove(&terminal_id);
        registry.panes.remove(&format!("terminal_{}", terminal_id));
        registry.pending_panes.remove(&format!("terminal_{}", terminal_id));
    }
}

pub(crate) fn ensure_tab(tab_id: usize, name: &str) {
    if let Some(registry) = registry() { registry.lock().unwrap().ensure_tab(tab_id, name); }
}

fn pane_key(pane_id: PaneId) -> String {
    match pane_id { PaneId::Terminal(id) => format!("terminal_{}", id), PaneId::Plugin(id) => format!("plugin_{}", id) }
}

/// Called by Screen before publishing topology or dispatching another action.
pub(crate) fn synchronize<'a>(tabs: impl Iterator<Item = (usize, &'a str, Vec<PaneId>)>, clients: &BTreeMap<ClientId, usize>) {
    let Some(registry) = registry() else { return; };
    registry.lock().unwrap().synchronize(tabs, clients);
}

pub(crate) fn set_clients(clients: &BTreeMap<ClientId, usize>) {
    if let Some(registry) = registry() {
        let mut registry = registry.lock().unwrap();
        if registry.clients != *clients { registry.clients.clone_from(clients); }
    }
}

pub(crate) fn event() -> Option<Event> { registry().map(|r| r.lock().unwrap().event()) }

pub(crate) fn update(plugin_id: u32, token: &str, payload: &str) -> bool {
    let Some(registry) = registry() else { return false; };
    let Ok(update) = serde_json::from_str(payload) else { return false; };
    registry.lock().unwrap().update(plugin_id, token, update)
}

/// Only the session's original load_plugins configuration may establish authority.
pub(crate) fn register_startup_owner(plugin: &RunPluginOrAlias) {
    let Some(registry) = registry() else { return; };
    let Some(run) = plugin.get_run_plugin() else { return; };
    let config = run.configuration.inner();
    let mut registry = registry.lock().unwrap();
    if registry.owner_configuration.is_none()
        && config.get("role").map(String::as_str) == Some("native-owner")
        && config.get("session_id") == Some(&registry.session_id)
    {
        if let Some(token) = config.get("context_owner").filter(|token| !token.is_empty()) {
            registry.owner_token.clone_from(token);
            registry.owner_configuration = Some(plugin.clone());
        }
    }
}

pub(crate) fn owner_loaded(plugin: &RunPluginOrAlias, plugin_id: u32) {
    let Some(registry) = registry() else { return; };
    let mut registry = registry.lock().unwrap();
    if registry.owner_configuration.as_ref() == Some(plugin) {
        registry.owner_plugin = Some(plugin_id);
        registry.owner_alive = true;
        registry.restart_pending = false;
    }
}

pub(crate) fn owner_unloaded(plugin_id: u32) {
    if let Some(registry) = registry() {
        let mut registry = registry.lock().unwrap();
        if registry.owner_plugin == Some(plugin_id) { registry.owner_alive = false; }
    }
}

pub(crate) fn restart_owner(session_id: &str, closed_owner: u32, source: &RunPluginLocation) -> Option<RunPluginOrAlias> {
    registry()?.lock().unwrap().restart_owner(session_id, closed_owner, source)
}

pub(crate) fn owner_load_failed(plugin: &RunPluginOrAlias) {
    if let Some(registry) = registry() {
        let mut registry = registry.lock().unwrap();
        if registry.owner_configuration.as_ref() == Some(plugin) { registry.restart_pending = false; }
    }
}

/// Remove inherited Bkpr authority at every unmanaged/background child boundary.
pub(crate) fn clear_environment(command: &mut std::process::Command) -> &mut std::process::Command {
    for key in CONTEXT_ENV { command.env_remove(key); }
    command.env_remove("BKPR_CONTEXT_OWNER").env_remove("BKPR_NATIVE_CONTEXT")
}

/// Managed terminals replace the cleared namespace with their destination context.
pub(crate) fn apply_environment(command: &mut std::process::Command, terminal_id: u32) {
    if let Some(registry) = registry() {
        registry.lock().unwrap().apply_environment(command, terminal_id);
    } else {
        clear_environment(command);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Registry {
        let mut registry = Registry::new("incarnation-a".into(), "work".into(), "secret".into());
        registry.owner_plugin = Some(2);
        registry.owner_alive = true;
        registry
    }

    #[test]
    fn stock_new_tabs_are_distinct_and_reused_session_names_do_not_reuse_ids() {
        let mut first = registry();
        first.ensure_tab(0, "one"); first.ensure_tab(1, "two");
        assert_ne!(first.tabs[&0].context.workspace_id, first.tabs[&1].context.workspace_id);
        let mut next = Registry::new("incarnation-b".into(), "work".into(), "secret".into());
        next.ensure_tab(0, "one");
        assert_ne!(first.tabs[&0].context.workspace_id, next.tabs[&0].context.workspace_id);
    }

    #[test]
    fn explicit_grouping_preserves_workspace_name_across_native_tab_names() {
        let mut registry = registry();
        let mut layout = TiledPaneLayout::default();
        layout.context_metadata.insert("bkpr_workspace_id".into(), "group".into());
        layout.context_metadata.insert("bkpr_workspace_name".into(), "workspace".into());
        registry.bind_layout(0, &layout); registry.bind_layout(1, &layout);
        assert_eq!(registry.tabs[&0].context.workspace_id, registry.tabs[&1].context.workspace_id);
        assert_eq!(registry.tabs[&0].context.workspace_name, "workspace");
        registry.synchronize(vec![(0, "native one", vec![]), (1, "native two", vec![])].into_iter(), &BTreeMap::new());
        assert_eq!(registry.tabs[&1].context.workspace_name, "workspace");
        assert!(registry.update(2, "secret", Update {
            session_id: "incarnation-a".into(), generation: 1,
            tabs: vec![TabContext { tab_id: 0, workspace_id: "group".into(), workspace_name: "renamed group".into() }],
        }));
        assert_eq!(registry.tabs[&1].context.workspace_name, "renamed group");
    }

    #[test]
    fn updates_reject_foreign_sessions_tokens_owners_and_stale_generations() {
        let mut registry = registry(); registry.ensure_tab(0, "one");
        let update = |session: &str, generation| Update { session_id: session.into(), generation,
            tabs: vec![TabContext { tab_id: 0, workspace_id: "group".into(), workspace_name: "renamed".into() }] };
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
        command.get_envs().map(|(key, value)| (key.to_string_lossy().into_owned(),
            value.map(|value| value.to_string_lossy().into_owned()))).collect()
    }

    #[test]
    fn direct_program_and_stock_split_receive_all_five_values_without_a_shell() {
        let mut registry = registry();
        registry.ensure_tab(0, "Workspace");
        registry.clients.insert(7, 0);
        registry.reserve_terminal(0, 10);
        let source = registry.resolve_target(ClientTabIndexOrPaneId::ClientId(7)).unwrap();
        registry.reserve_terminal(source, 11);
        for id in [10, 11] {
            let mut command = std::process::Command::new("/usr/bin/env");
            for key in CONTEXT_ENV { command.env(key, "stale-parent"); }
            command.env("BKPR_CONTEXT_OWNER", "secret");
            registry.apply_environment(&mut command, id);
            let env = environment(&command);
            assert_eq!(env["BKPR_SESSION_ID"].as_deref(), Some("incarnation-a"));
            assert_eq!(env["BKPR_SESSION_NAME"].as_deref(), Some("work"));
            assert_eq!(env["BKPR_WORKSPACE_ID"].as_deref(), Some("incarnation-a:workspace:0"));
            assert_eq!(env["BKPR_WORKSPACE_NAME"].as_deref(), Some("Workspace"));
            assert_eq!(env["BKPR_PANE_ID"].as_deref(), Some(format!("terminal_{}", id).as_str()));
            assert_eq!(env["BKPR_CONTEXT_OWNER"], None);
        }
    }

    #[test]
    fn move_rename_close_and_future_children_follow_live_topology_not_old_environment() {
        let mut registry = registry();
        registry.ensure_tab(0, "one"); registry.ensure_tab(1, "two");
        registry.reserve_terminal(0, 10);
        let mut original = std::process::Command::new("/usr/bin/env");
        registry.apply_environment(&mut original, 10);
        registry.synchronize(vec![(0, "one", vec![]), (1, "renamed", vec![PaneId::Terminal(10)])].into_iter(), &BTreeMap::new());
        let destination = registry.resolve_target(ClientTabIndexOrPaneId::PaneId(PaneId::Terminal(10))).unwrap();
        registry.reserve_terminal(destination, 11);
        let mut child = std::process::Command::new("/usr/bin/env");
        registry.apply_environment(&mut child, 11);
        assert_eq!(environment(&original)["BKPR_WORKSPACE_NAME"].as_deref(), Some("one"));
        assert_eq!(environment(&child)["BKPR_WORKSPACE_NAME"].as_deref(), Some("renamed"));
        assert_eq!(environment(&child)["BKPR_WORKSPACE_ID"].as_deref(), Some("incarnation-a:workspace:1"));
        registry.synchronize(vec![(1, "renamed", vec![])].into_iter(), &BTreeMap::new());
        assert!(!registry.panes.contains_key("terminal_10"));
        assert!(!registry.tabs.contains_key(&0));
    }

    #[test]
    fn recovery_reuses_trusted_configuration_once_and_rejects_foreign_or_live_owners() {
        let mut registry = registry();
        let source = RunPluginLocation::File(std::path::PathBuf::from("/tmp/bkpr.wasm"));
        let template = RunPluginOrAlias::RunPlugin(zellij_utils::input::layout::RunPlugin {
            location: source.clone(), ..Default::default()
        });
        registry.owner_configuration = Some(template.clone());
        assert!(registry.restart_owner("incarnation-a", 2, &source).is_none());
        registry.owner_alive = false;
        assert!(registry.restart_owner("foreign", 2, &source).is_none());
        assert!(registry.restart_owner("incarnation-a", 1, &source).is_none());
        let foreign = RunPluginLocation::File(std::path::PathBuf::from("/tmp/foreign.wasm"));
        assert!(registry.restart_owner("incarnation-a", 2, &foreign).is_none());
        assert_eq!(registry.restart_owner("incarnation-a", 2, &source), Some(template));
        assert!(registry.restart_owner("incarnation-a", 2, &source).is_none());
        registry.owner_plugin = Some(3);
        registry.owner_alive = true;
        registry.restart_pending = false;
        assert!(registry.restart_owner("incarnation-a", 2, &source).is_none());
    }

    #[test]
    fn native_snapshot_carries_live_membership_and_root_pid_without_authority_secrets() {
        let mut registry = registry();
        registry.ensure_tab(0, "Workspace");
        registry.reserve_terminal(0, 10);
        registry.root_pids.insert(10, 1234);
        let Event::CustomMessage(name, payload) = registry.event() else { panic!("expected context"); };
        assert_eq!(name, "bkpr_context");
        let snapshot: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(snapshot["session_id"], "incarnation-a");
        assert_eq!(snapshot["panes"][0]["pane_id"], "terminal_10");
        assert_eq!(snapshot["panes"][0]["root_pid"], 1234);
        assert_eq!(snapshot["panes"][0]["workspace_name"], "Workspace");
        assert!(!payload.contains("secret"));
        assert!(!payload.contains("context_owner"));
    }
}

#[cfg(test)]
mod environment_boundary_tests {
    use super::*;

    #[test]
    fn unmanaged_nested_runtime_clears_context_without_touching_other_environment() {
        let mut command = std::process::Command::new("/usr/bin/env");
        for key in CONTEXT_ENV { command.env(key, "outer-session"); }
        command.env("BKPR_NATIVE_CONTEXT", "1").env("BKPR_CONTEXT_OWNER", "secret");
        command.env("UNRELATED", "preserved");
        clear_environment(&mut command);
        for (key, value) in command.get_envs() {
            if key == "UNRELATED" { assert_eq!(value, Some(std::ffi::OsStr::new("preserved"))); }
            else { assert!(value.is_none(), "inherited Bkpr namespace leaked"); }
        }
    }
}
