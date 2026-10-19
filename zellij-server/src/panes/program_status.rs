//! Terminal-owned OSC 7501 records. Presentation and application recognition are not parser concerns.
use base64::{engine::{general_purpose::GeneralPurpose, DecodePaddingMode, GeneralPurposeConfig}, alphabet::STANDARD, Engine};
use zellij_utils::data::ProgramStatus;

const DECODER: GeneralPurpose = GeneralPurpose::new(&STANDARD, GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent));

#[derive(Clone, Default)]
pub struct ProgramStatuses {
    // Oldest update first; replacing a record also refreshes its eviction position.
    records: Vec<ProgramStatus>,
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
    pub fn clear(&mut self) { self.records.clear(); }

    pub fn expire_running(&mut self) {
        self.records.retain(|record| !matches!(record.state.as_str(), "working" | "blocked"));
    }

    pub fn snapshot(&self) -> Vec<ProgramStatus> {
        self.records.iter().map(|record| {
            let mut result = record.clone();
            let mut ancestor = record.id.as_str();
            while result.app.is_none() && !ancestor.is_empty() {
                ancestor = ancestor.rsplit_once('/').map_or("", |(parent, _)| parent);
                result.app = self.records.iter().find(|r| r.id == ancestor).and_then(|r| r.app.clone());
            }
            result
        }).collect()
    }

    pub fn report(&mut self, body: &[u8]) {
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
            self.records.retain(|r| !record.id.is_empty() && r.id != record.id && !r.id.strip_prefix(&record.id).is_some_and(|suffix| suffix.starts_with('/')));
            return;
        }
        record.state = state.to_owned();
        if state == "blocked" {
            record.kind = kind.filter(|kind| matches!(*kind, "permission" | "question" | "auth")).map(str::to_owned);
        }
        if matches!(state, "working" | "blocked") {
            record.progress = progress.filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())).and_then(|p| p.parse::<u8>().ok()).filter(|p| *p <= 100);
        }
        self.records.retain(|r| r.id != record.id);
        if self.records.len() == 256 { self.records.remove(0); }
        self.records.push(record);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_replace_inherit_clear_and_expire_by_lifetime() {
        let mut records = ProgramStatuses::default();
        records.report(b"state=working:app=cargo:msg=QnVpbGQ=");
        records.report(b"state=done:id=build:app=test");
        records.report(b"state=blocked:id=build/check:kind=permission:progress=50");
        assert_eq!(records.snapshot()[2].app.as_deref(), Some("test"));
        records.report(b"state=error:id=build");
        assert_eq!(records.snapshot()[1].app.as_deref(), Some("cargo"));
        assert_eq!(records.snapshot()[2].msg, None);
        records.expire_running();
        assert_eq!(records.snapshot().iter().map(|r| r.state.as_str()).collect::<Vec<_>>(), vec!["error"]);
        records.report(b"state=done:id=build/child");
        records.report(b"state=done:id=builder");
        records.report(b"state=clear:id=build");
        assert_eq!(records.snapshot()[0].id, "builder");
        records.clear();
        assert!(records.snapshot().is_empty());
    }

    #[test]
    fn hostile_reports_are_atomic_and_bad_pairs_do_not_poison_valid_pairs() {
        let mut records = ProgramStatuses::default();
        records.report(b"state=done:app=cargo");
        for bad in ["state=working:title=a", "state=blocked:msg=Gwo=", "state=error:title=wog=", "state=unknown", "id=bad//path:state=idle", "id=:state=clear", "id=bad?:state=clear", "state=working:msg=bad="] {
            records.report(bad.as_bytes());
            assert_eq!(records.snapshot()[0].state, "done", "{bad}");
        }
        for bad in [format!("state=clear:title={}", "YQ".repeat(129)), format!("state=clear:app={}", "a".repeat(33)), format!("state=clear:id={}", "a/".repeat(9)), format!("state=clear:{}=x", "a".repeat(17)), format!("state=clear:unknown={}", "a".repeat(4096))] {
            records.report(bad.as_bytes());
            assert_eq!(records.snapshot()[0].state, "done");
        }
        records.report(b"not-a-pair:=oops:bad=!:future=allowed:state=error:state=working:progress=101:kind=question:app=invalid,app:title=VGVzdA");
        let record = &records.snapshot()[0];
        assert_eq!(record.state, "working");
        assert_eq!(record.title.as_deref(), Some("Test"));
        assert_eq!((record.app.as_ref(), record.kind.as_ref(), record.progress), (None, None, None));
        records.report(b"state=clear:id=\xff");
        assert_eq!(records.snapshot()[0].state, "working");
        records.report(b"state=blocked:id=bad//path:id=good");
        assert!(records.snapshot().iter().any(|record| record.id == "good" && record.state == "blocked"));
        records.report(b"state=clear:id=good:id=bad?");
        assert!(records.snapshot().iter().any(|record| record.id == "good" && record.state == "blocked"));
        records.report(b"state=clear:id=bad?:id=good");
        assert!(!records.snapshot().iter().any(|record| record.id == "good"));
    }

    #[test]
    fn record_cap_evicts_least_recently_updated_not_oldest_created() {
        let mut records = ProgramStatuses::default();
        for id in 0..256 { records.report(format!("state=done:id={id}").as_bytes()); }
        records.report(b"state=error:id=0");
        records.report(b"state=done:id=256");
        let snapshot = records.snapshot();
        assert_eq!(snapshot.len(), 256);
        assert!(!snapshot.iter().any(|r| r.id == "1"));
        assert!(snapshot.iter().any(|r| r.id == "0" && r.state == "error"));
    }
}
