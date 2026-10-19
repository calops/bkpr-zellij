//! Terminal-owned OSC 7501 records. Presentation and application recognition are not parser concerns.
use base64::{engine::{general_purpose::GeneralPurpose, DecodePaddingMode, GeneralPurposeConfig}, alphabet::STANDARD, Engine};
use zellij_utils::data::{Event, ProgramStatus};

const DECODER: GeneralPurpose = GeneralPurpose::new(&STANDARD, GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent));

#[derive(Clone, Default)]
pub struct ProgramStatuses {
    // Oldest update first; replacing a record also refreshes its eviction position.
    records: Vec<ProgramStatus>,
    changed: bool,
    owner_process_group: Option<i32>,
}

fn segment(value: &str) -> bool {
    !value.is_empty() && value.len() <= 32 && value.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.+-".contains(&b))
}

fn text(value: &str, limit: usize) -> Option<String> {
    let bytes = DECODER.decode(value).ok()?;
    if bytes.len() > limit { return None; }
    let text = String::from_utf8(bytes).ok()?;
    if text.chars().any(|c| c.is_control()) { return None; }
    Some(text)
}

impl ProgramStatuses {
    pub fn clear(&mut self) {
        self.changed |= !self.records.is_empty();
        self.records.clear();
        self.owner_process_group = None;
    }

    pub fn expire_running(&mut self) {
        let previous_len = self.records.len();
        self.records.retain(|record| !matches!(record.state.as_str(), "working" | "blocked"));
        self.changed |= self.records.len() != previous_len;
    }

    pub fn expire_running_on_prompt(&mut self, read_foreground: &dyn Fn() -> Option<i32>) {
        // Applications also mark transcript blocks with OSC 133. Only a proven
        // foreground ownership change makes their prompt a shell-return boundary.
        if !self.records.iter().any(|record| matches!(record.state.as_str(), "working" | "blocked")) {
            return;
        }
        if let Some(owner) = self.owner_process_group {
            if let Some(foreground) = read_foreground().filter(|group| *group > 0) {
                if owner != foreground {
                    self.expire_running();
                }
            }
        }
    }

    // Ordinary terminal output and duplicate reports never allocate a snapshot.
    pub fn take_change(&mut self) -> Option<Vec<ProgramStatus>> {
        std::mem::take(&mut self.changed).then(|| self.snapshot())
    }

    fn effective_app<'a>(&'a self, record: &'a ProgramStatus) -> Option<&'a str> {
        if let Some(app) = record.app.as_deref() {
            return Some(app);
        }
        let mut ancestor = record.id.as_str();
        while !ancestor.is_empty() {
            ancestor = ancestor.rsplit_once('/').map_or("", |(parent, _)| parent);
            if let Some(app) = self.records.iter().find(|r| r.id == ancestor).and_then(|r| r.app.as_deref()) {
                return Some(app);
            }
        }
        None
    }

    pub fn snapshot(&self) -> Vec<ProgramStatus> {
        self.records.iter().map(|record| {
            let mut result = record.clone();
            if result.app.is_none() {
                result.app = self.effective_app(record).map(str::to_owned);
            }
            result
        }).collect()
    }

    pub fn report(&mut self, body: &[u8], read_foreground: &dyn Fn() -> Option<i32>) {
        // ST can be two bytes. Accepting the conservative cap also bounds all work below.
        if body.len() + 9 > 4096 { return; }
        let mut record = ProgramStatus::default();
        let mut state = None;
        let mut id = None;
        let mut kind = None;
        let mut progress = None;
        for pair in body.split(|byte| *byte == b':') {
            let Some(equals) = pair.iter().position(|byte| *byte == b'=') else {
                if pair.trim_ascii().len() > 16 { return; }
                continue;
            };
            let key = pair[..equals].trim_ascii();
            let value = pair[equals + 1..].trim_ascii();
            if key.len() > 16 { return; }
            let Ok(key) = std::str::from_utf8(key) else { continue; };
            // Limits apply to every pair, even unknown/malformed or superseded pairs.
            if match key {
                "msg" => value.len() > 2732,
                "title" => value.len() > 256,
                "app" => value.len() > 32,
                "id" => value.len() > 128 || value.split(|b| *b == b'/').count() > 8 || value.split(|b| *b == b'/').any(|s| s.len() > 32),
                _ => false,
            } { return; }
            if key == "id" { id = Some(value); continue; }
            let Ok(value) = std::str::from_utf8(value) else { continue; };
            if key.is_empty() || !key.bytes().all(|b| b.is_ascii_lowercase()) || !value.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.,+/=-".contains(&b)) { continue; }
            match key {
                "state" => state = Some(value),
                "kind" => kind = Some(value),
                "progress" => progress = Some(value),
                "app" => record.app = segment(value).then(|| value.to_owned()),
                "title" => { let Some(decoded) = text(value, 192) else { return; }; record.title = Some(decoded); },
                "msg" => { let Some(decoded) = text(value, 2048) else { return; }; record.msg = Some(decoded); },
                _ => {},
            }
        }
        let Some(state) = state else { return; };
        if !matches!(state, "idle" | "working" | "done" | "blocked" | "error" | "clear") { return; }
        if let Some(id) = id {
            let Ok(id) = std::str::from_utf8(id) else { return; };
            if !id.split('/').all(segment) { return; }
            record.id = id.to_owned();
        }
        if state == "clear" {
            let previous_len = self.records.len();
            self.records.retain(|r| !record.id.is_empty() && r.id != record.id && !r.id.strip_prefix(&record.id).is_some_and(|suffix| suffix.starts_with('/')));
            self.changed |= self.records.len() != previous_len;
            if record.id.is_empty() {
                self.owner_process_group = None;
            }
            return;
        }
        if record.id.is_empty() || self.owner_process_group.is_none() {
            self.owner_process_group = read_foreground().filter(|group| *group > 0);
        }
        record.state = state.to_owned();
        if state == "blocked" {
            record.kind = kind.filter(|kind| matches!(*kind, "permission" | "question" | "auth")).map(str::to_owned);
        }
        if matches!(state, "working" | "blocked") {
            record.progress = progress.filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())).and_then(|p| p.parse::<u8>().ok()).filter(|p| *p <= 100);
        }
        if let Some(index) = self.records.iter().position(|r| r.id == record.id) {
            let previous = &self.records[index];
            let changed = previous.state != record.state
                || previous.kind != record.kind
                || previous.progress != record.progress
                || previous.title != record.title
                || previous.msg != record.msg
                || self.effective_app(previous) != self.effective_app(&record);
            self.changed |= changed;
            self.records.remove(index);
        } else {
            self.changed = true;
        }
        if self.records.len() == 256 { self.records.remove(0); }
        self.records.push(record);
    }
}

pub fn status_event(pane_id: u32, records: &[ProgramStatus]) -> serde_json::Result<Event> {
    #[derive(serde::Serialize)]
    struct Update<'a> {
        pane_id: u32,
        records: &'a [ProgramStatus],
    }
    serde_json::to_string(&Update { pane_id, records })
        .map(|payload| Event::CustomMessage("program_status".to_owned(), payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_replace_inherit_clear_and_expire_by_lifetime() {
        let mut records = ProgramStatuses::default();
        records.report(b"state=working:app=cargo:msg=QnVpbGQ=", &|| None);
        records.report(b"state=done:id=build:app=test", &|| None);
        records.report(b"state=blocked:id=build/check:kind=permission:progress=50", &|| None);
        assert_eq!(records.snapshot()[2].app.as_deref(), Some("test"));
        records.report(b"state=error:id=build", &|| None);
        assert_eq!(records.snapshot()[1].app.as_deref(), Some("cargo"));
        assert_eq!(records.snapshot()[2].msg, None);
        records.expire_running();
        assert_eq!(records.snapshot().iter().map(|r| r.state.as_str()).collect::<Vec<_>>(), vec!["error"]);
        records.report(b"state=done:id=build/child", &|| None);
        records.report(b"state=done:id=builder", &|| None);
        records.report(b"state=clear:id=build", &|| None);
        assert_eq!(records.snapshot()[0].id, "builder");
        records.clear();
        assert!(records.snapshot().is_empty());
    }

    #[test]
    fn hostile_reports_are_atomic_and_bad_pairs_do_not_poison_valid_pairs() {
        let mut records = ProgramStatuses::default();
        records.report(b"state=done:app=cargo", &|| None);
        for bad in ["state=working:title=a", "state=blocked:msg=Gwo=", "state=error:title=wog=", "state=unknown", "id=bad//path:state=idle", "id=:state=clear", "id=bad?:state=clear", "state=working:msg=bad="] {
            records.report(bad.as_bytes(), &|| None);
            assert_eq!(records.snapshot()[0].state, "done", "{bad}");
        }
        for bad in [format!("state=clear:title={}", "YQ".repeat(129)), format!("state=clear:app={}", "a".repeat(33)), format!("state=clear:id={}", "a/".repeat(9)), format!("state=clear:{}=x", "a".repeat(17)), format!("state=clear:unknown={}", "a".repeat(4096))] {
            records.report(bad.as_bytes(), &|| None);
            assert_eq!(records.snapshot()[0].state, "done");
        }
        records.report(b"not-a-pair:=oops:bad=!:future=allowed:state=error:state=working:progress=101:kind=question:app=invalid,app:title=VGVzdA", &|| None);
        let record = &records.snapshot()[0];
        assert_eq!(record.state, "working");
        assert_eq!(record.title.as_deref(), Some("Test"));
        assert_eq!((record.app.as_ref(), record.kind.as_ref(), record.progress), (None, None, None));
        records.report(b"state=clear:id=\xff", &|| None);
        assert_eq!(records.snapshot()[0].state, "working");
        records.report(b"state=blocked:id=bad//path:id=good", &|| None);
        assert!(records.snapshot().iter().any(|record| record.id == "good" && record.state == "blocked"));
        records.report(b"state=clear:id=good:id=bad?", &|| None);
        assert!(records.snapshot().iter().any(|record| record.id == "good" && record.state == "blocked"));
        records.report(b"state=clear:id=bad?:id=good", &|| None);
        assert!(!records.snapshot().iter().any(|record| record.id == "good"));
    }

    #[test]
    fn record_cap_evicts_least_recently_updated_not_oldest_created() {
        let mut records = ProgramStatuses::default();
        for id in 0..256 { records.report(format!("state=done:id={id}").as_bytes(), &|| None); }
        records.report(b"state=error:id=0", &|| None);
        records.report(b"state=done:id=256", &|| None);
        let snapshot = records.snapshot();
        assert_eq!(snapshot.len(), 256);
        assert!(!snapshot.iter().any(|r| r.id == "1"));
        assert!(snapshot.iter().any(|r| r.id == "0" && r.state == "error"));
    }

    #[test]
    fn duplicate_updates_refresh_eviction_without_publishing_unchanged_records() {
        let mut records = ProgramStatuses::default();
        for id in 0..256 { records.report(format!("state=done:id={id}").as_bytes(), &|| None); }
        assert_eq!(records.take_change().unwrap().len(), 256);
        records.report(b"state=done:id=0", &|| None);
        assert!(records.take_change().is_none());
        records.report(b"state=done:id=256", &|| None);
        let changed = records.take_change().unwrap();
        assert!(!changed.iter().any(|r| r.id == "1"));
        assert!(changed.iter().any(|r| r.id == "0"));
    }

    #[test]
    fn clears_expiry_and_inherited_apps_publish_only_changed_values() {
        let mut records = ProgramStatuses::default();
        assert!(records.take_change().is_none());
        records.clear();
        records.expire_running();
        records.report(b"state=clear:id=absent", &|| None);
        records.report(b"state=unknown", &|| None);
        records.report(b"state=working:title=a", &|| None);
        assert!(records.take_change().is_none());
        records.report(b"state=done:app=cargo", &|| None);
        records.report(b"state=working:id=build", &|| None);
        assert_eq!(records.take_change().unwrap()[1].app.as_deref(), Some("cargo"));
        records.report(b"state=working:id=build:app=cargo", &|| None);
        assert!(records.take_change().is_none());
        records.report(b"state=error:app=test", &|| None);
        let changed = records.take_change().unwrap();
        assert_eq!(changed.iter().find(|r| r.id == "build").unwrap().app.as_deref(), Some("cargo"));
        records.expire_running();
        assert_eq!(records.take_change().unwrap()[0].state, "error");
        records.expire_running();
        assert!(records.take_change().is_none());
        records.clear();
        assert!(records.take_change().unwrap().is_empty());
        records.clear();
        assert!(records.take_change().is_none());
    }
}
