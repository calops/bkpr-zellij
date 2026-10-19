//! Bound raw OSC 7501 and Kitty OSC 5522 before vte's unbounded OSC buffer.
//! Other terminal sequences still go through vte, unchanged.
use super::grid::Grid;
use vte::{Params, Perform};

#[derive(Clone, Copy)]
enum Mode {
    Ground,
    Escape,
    Prefix(usize),
    ClipboardPrefix(usize),
    Clipboard { escape: bool, discarded: bool },
    OtherString { bell: bool, escape: bool },
    Status { escape: bool, discarded: bool },
}

fn after_escape(byte: u8) -> Mode {
    match byte {
        b']' => Mode::Prefix(0),
        b'P' | b'_' | b'^' | b'X' => Mode::OtherString { bell: false, escape: false },
        0x1b => Mode::Escape,
        _ => Mode::Ground,
    }
}

struct ProgramStatusPerformer<'a> {
    grid: &'a mut Grid,
    read_foreground: &'a dyn Fn() -> Option<i32>,
}

impl Perform for ProgramStatusPerformer<'_> {
    fn print(&mut self, c: char) { self.grid.print(c); }
    fn execute(&mut self, byte: u8) { self.grid.execute(byte); }
    fn hook(&mut self, params: &Params, intermediates: &[u8], ignore: bool, c: char) {
        self.grid.hook(params, intermediates, ignore, c);
    }
    fn put(&mut self, byte: u8) { self.grid.put(byte); }
    fn unhook(&mut self) { self.grid.unhook(); }
    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        if params.len() == 2 && params[0] == b"7501" && params[1] != b"?" {
            self.grid.program_status.report(params[1], self.read_foreground);
            return;
        }
        if params.first() == Some(&b"133".as_slice()) && params.get(1) == Some(&b"A".as_slice()) {
            self.grid.program_status.expire_running_on_prompt(self.read_foreground);
        }
        self.grid.osc_dispatch(params, bell_terminated);
    }
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, c: char) {
        self.grid.csi_dispatch(params, intermediates, ignore, c);
    }
    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        self.grid.esc_dispatch(intermediates, ignore, byte);
    }
    fn terminated(&self) -> bool { self.grid.terminated() }
}

pub struct ProgramStatusVteParser {
    parser: vte::Parser,
    mode: Mode,
    body: Vec<u8>,
}

impl ProgramStatusVteParser {
    pub fn new() -> Self {
        Self { parser: vte::Parser::new(), mode: Mode::Ground, body: Vec::new() }
    }

    pub fn advance_until_terminated(&mut self, grid: &mut Grid, bytes: &[u8], read_foreground: &dyn Fn() -> Option<i32>) -> usize {
        let mut performer = ProgramStatusPerformer { grid, read_foreground };
        let mut index = 0;
        while index < bytes.len() {
            // Ordinary grid text remains a single bulk parser call, not per-byte dispatch.
            if matches!(self.mode, Mode::Ground) && bytes[index] != 0x1b {
                let end = bytes[index..].iter().position(|b| *b == 0x1b).map_or(bytes.len(), |n| index + n);
                let consumed = self.parser.advance_until_terminated(&mut performer, &bytes[index..end]);
                index += consumed;
                if index < end || !performer.grid.pending_forwarded_queries.is_empty() { return index; }
                continue;
            }
            if let Mode::OtherString { bell, escape: false } = self.mode {
                if !matches!(bytes[index], 0x1b | 0x18 | 0x1a) && (!bell || bytes[index] != 7) {
                    let end = bytes[index..].iter().position(|b| matches!(*b, 0x1b | 0x18 | 0x1a) || (bell && *b == 7)).map_or(bytes.len(), |n| index + n);
                    let consumed = self.parser.advance_until_terminated(&mut performer, &bytes[index..end]);
                    index += consumed;
                    if index < end || !performer.grid.pending_forwarded_queries.is_empty() { return index; }
                    continue;
                }
            }
            let byte = bytes[index];
            let previous = self.mode;
            let mut forward = true;
            match self.mode {
                Mode::Ground => self.mode = Mode::Escape,
                Mode::Escape => self.mode = after_escape(byte),
                Mode::Prefix(n) => {
                    if n == 0 && byte == b'5' {
                        self.mode = Mode::ClipboardPrefix(1);
                    } else if byte == b"7501;"[n] {
                        if n == 4 {
                            // The already-forwarded prefix cannot dispatch anything; cancel it.
                            self.parser.advance(&mut performer, b"\x18");
                            self.body.clear();
                            self.mode = Mode::Status { escape: false, discarded: false };
                            forward = false;
                        } else { self.mode = Mode::Prefix(n + 1); }
                    } else {
                        self.mode = if matches!(byte, 7 | 0x18 | 0x1a) { Mode::Ground }
                            else { Mode::OtherString { bell: true, escape: byte == 0x1b } };
                    }
                },
                Mode::ClipboardPrefix(n) => {
                    if byte == b"5522;"[n] {
                        if n == 4 {
                            self.parser.advance(&mut performer, b"\x18");
                            self.body.clear();
                            self.body.extend_from_slice(b"5522;");
                            self.mode = Mode::Clipboard { escape: false, discarded: false };
                            forward = false;
                        } else { self.mode = Mode::ClipboardPrefix(n + 1); }
                    } else {
                        self.mode = if matches!(byte, 7 | 0x18 | 0x1a) { Mode::Ground }
                            else { Mode::OtherString { bell: true, escape: byte == 0x1b } };
                    }
                },
                Mode::Clipboard { escape, discarded } => {
                    forward = false;
                    if byte == 7 || (escape && byte == b'\\') {
                        if !discarded {
                            let params = self.body.split(|b| *b == b';').collect::<Vec<_>>();
                            performer.osc_dispatch(&params, byte == 7);
                        }
                        self.body.clear();
                        self.mode = Mode::Ground;
                    } else if matches!(byte, 0x18 | 0x1a) {
                        if !discarded { performer.grid.reject_kitty_clipboard_packet("EINVAL"); }
                        self.body.clear();
                        self.mode = Mode::Ground;
                    } else if escape {
                        if !discarded { performer.grid.reject_kitty_clipboard_packet("EINVAL"); }
                        self.body.clear();
                        self.parser.advance(&mut performer, b"\x1b");
                        self.mode = Mode::Escape;
                        continue;
                    } else if byte == 0x1b {
                        self.mode = Mode::Clipboard { escape: true, discarded };
                    } else if !discarded {
                        if self.body.len() == zellij_utils::kitty_clipboard::MAX_PACKET_BYTES {
                            performer.grid.reject_kitty_clipboard_packet("EFBIG");
                            self.body.clear();
                            self.mode = Mode::Clipboard { escape: false, discarded: true };
                        } else { self.body.push(byte); }
                    }
                },
                Mode::OtherString { bell, escape } => {
                    self.mode = if (bell && byte == 7) || (escape && byte == b'\\') || matches!(byte, 0x18 | 0x1a) { Mode::Ground }
                        else if escape { after_escape(byte) }
                        else { Mode::OtherString { bell, escape: byte == 0x1b } };
                },
                Mode::Status { escape, discarded } => {
                    forward = false;
                    if byte == 7 || (escape && byte == b'\\') {
                        let overhead = if byte == 7 { 8 } else { 9 };
                        if !discarded && self.body.len() + overhead <= 4096 {
                            performer.osc_dispatch(&[b"7501", &self.body], byte == 7);
                        }
                        self.body.clear();
                        self.mode = Mode::Ground;
                    } else if escape {
                        // An unexpected ESC starts another terminal sequence, not a valid ST.
                        self.body.clear();
                        self.parser.advance(&mut performer, b"\x1b");
                        self.mode = Mode::Escape;
                        // Reprocess this byte normally as the character following that ESC.
                        continue;
                    } else if byte == 0x1b {
                        self.mode = Mode::Status { escape: true, discarded };
                    } else if !discarded {
                        if self.body.len() == 4088 {
                            self.body.clear();
                            self.mode = Mode::Status { escape: false, discarded: true };
                        } else { self.body.push(byte); }
                    }
                },
            }
            if forward {
                if self.parser.advance_until_terminated(&mut performer, &bytes[index..index + 1]) == 0 {
                    self.mode = previous;
                    return index;
                }
            }
            index += 1;
            if !performer.grid.pending_forwarded_queries.is_empty() { return index; }
        }
        index
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::HashMap, rc::Rc};
    use crate::panes::{sixel::SixelImageStore, kitty_graphics::KittyImageStore, link_handler::LinkHandler};
    use zellij_utils::data::{Palette, Style};

    fn grid() -> Grid {
        Grid::new(
            24, 80, Rc::new(RefCell::new(Palette::default())),
            Rc::new(RefCell::new(HashMap::new())), Rc::new(RefCell::new(LinkHandler::new())),
            Rc::new(RefCell::new(None)), Rc::new(RefCell::new(SixelImageStore::default())),
            Rc::new(RefCell::new(KittyImageStore::default())), Style::default(),
            false, true, true, true, false,
        )
    }

    #[test]
    fn actual_terminal_protocol_negotiates_preserves_screen_resets_and_expires_at_prompt() {
        let mut parser = ProgramStatusVteParser::new();
        let mut grid = grid();
        // Every byte may arrive separately from a PTY.
        for byte in b"\x1b]7501;?\x1b\\\x1b]7501;state=done:app=cargo\x07" {
            assert_eq!(parser.advance_until_terminated(&mut grid, &[*byte], &|| Some(200)), 1);
        }
        assert_eq!(grid.pending_messages_to_pty.remove(0), b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x1b\\");
        parser.advance_until_terminated(&mut grid, b"\x1b[?1049h\x1b[!p\x1b[?1049l", &|| None);
        assert_eq!(grid.program_status.snapshot()[0].state, "done");
        parser.advance_until_terminated(&mut grid, b"\x1b]7501;state=blocked:id=worker\x1b\\\x1b]133;A\x07", &|| Some(100));
        assert_eq!(grid.program_status.snapshot().iter().map(|r| r.state.as_str()).collect::<Vec<_>>(), vec!["done"]);
        parser.advance_until_terminated(&mut grid, b"\x1bc", &|| None);
        assert!(grid.program_status.snapshot().is_empty());
    }

    #[test]
    fn raw_reports_are_bounded_atomic_and_do_not_disrupt_following_terminal_sequences() {
        let mut parser = ProgramStatusVteParser::new();
        let mut grid = grid();
        parser.advance_until_terminated(&mut grid, b"\x1b]7501;state=done:app=cargo\x07", &|| None);
        let mut hostile = b"\x1b]7501;state=clear:unknown=".to_vec();
        hostile.extend(std::iter::repeat_n(b'a', 65536));
        hostile.extend_from_slice(b"\x1b\\\x1b]2;Ordinary window title\x07");
        assert_eq!(parser.advance_until_terminated(&mut grid, &hostile, &|| None), hostile.len());
        assert_eq!(grid.program_status.snapshot()[0].state, "done");
        assert!(parser.body.capacity() <= 8192);
        // Vte would strip raw controls from an OSC. The raw bridge must not turn a bad state pair into a valid one.
        parser.advance_until_terminated(&mut grid, b"\x1b]7501;state=cl\x01ear\x07", &|| None);
        assert_eq!(grid.program_status.snapshot()[0].state, "done");
        parser.advance_until_terminated(&mut grid, b"\x1b]7501;bad=\xff:state=working\x07", &|| None);
        assert_eq!(grid.program_status.snapshot()[0].state, "working");
        parser.advance_until_terminated(&mut grid, b"\x1b]7501;state=clear\x1b\\", &|| None);
        assert!(grid.program_status.snapshot().is_empty());
        parser.advance_until_terminated(&mut grid, b"\x1b]7501;state=done\x1b\\", &|| None);
        assert_eq!(grid.program_status.snapshot()[0].state, "done");
    }

    #[test]
    fn notifications_follow_completed_reports_and_terminal_lifetime() {
        let mut parser = ProgramStatusVteParser::new();
        let mut grid = grid();
        parser.advance_until_terminated(&mut grid, b"ordinary text\x1b]7501;?\x07\x1b]7501;state=unknown\x07", &|| None);
        assert!(grid.program_status.take_change().is_none());
        parser.advance_until_terminated(&mut grid, b"\x1b]7501;state=working:app=cargo", &|| None);
        assert!(grid.program_status.take_change().is_none());
        parser.advance_until_terminated(&mut grid, b"\x1b\\", &|| Some(200));
        assert_eq!(grid.program_status.take_change().unwrap()[0].state, "working");
        parser.advance_until_terminated(&mut grid, b"\x1b]7501;state=working:app=cargo\x07\x1b]7501;state=error:title=a\x07", &|| Some(200));
        assert!(grid.program_status.take_change().is_none());
        parser.advance_until_terminated(&mut grid, b"\x1b]7501;state=done:id=build\x07", &|| None);
        assert_eq!(grid.program_status.take_change().unwrap()[1].state, "done");
        parser.advance_until_terminated(&mut grid, b"\x1b]133;A\x07", &|| Some(100));
        let after_prompt = grid.program_status.take_change().unwrap();
        assert_eq!(after_prompt.len(), 1);
        assert_eq!(after_prompt[0].id, "build");
        parser.advance_until_terminated(&mut grid, b"\x1b]133;A\x07", &|| None);
        assert!(grid.program_status.take_change().is_none());
        parser.advance_until_terminated(&mut grid, b"\x1bc", &|| None);
        assert!(grid.program_status.take_change().unwrap().is_empty());
        parser.advance_until_terminated(&mut grid, b"\x1bc\x1b]7501;state=clear\x07", &|| None);
        assert!(grid.program_status.take_change().is_none());
    }

    #[test]
    fn reports_after_forwarded_queries_publish_only_after_resuming_parsing() {
        let mut parser = ProgramStatusVteParser::new();
        let mut grid = grid();
        let input = b"\x1b[14t\x1b]7501;state=working:app=cargo\x07";
        let consumed = parser.advance_until_terminated(&mut grid, input, &|| None);
        assert_eq!(consumed, b"\x1b[14t".len());
        assert_eq!(grid.pending_forwarded_queries.len(), 1);
        assert!(grid.program_status.take_change().is_none());
        grid.pending_forwarded_queries.clear();
        assert_eq!(parser.advance_until_terminated(&mut grid, &input[consumed..], &|| None), input.len() - consumed);
        assert_eq!(grid.program_status.take_change().unwrap()[0].state, "working");
    }

    #[test]
    fn native_titles_publish_changes_without_mutating_program_status() {
        let mut parser = ProgramStatusVteParser::new();
        let mut grid = grid();
        parser.advance_until_terminated(&mut grid, b"\x1b]7501;state=working:app=omp\x1b\\", &|| None);
        grid.program_status.take_change();
        parser.advance_until_terminated(&mut grid, b"\x1b]0;First task", &|| None);
        assert!(!grid.pending_title_changed);
        parser.advance_until_terminated(&mut grid, b"\x07", &|| None);
        assert!(std::mem::take(&mut grid.pending_title_changed));
        assert_eq!(grid.title.as_deref(), Some("First task"));
        parser.advance_until_terminated(&mut grid, b"\x1b]2;First task\x1b\\\x1b[22;0t", &|| None);
        assert!(!grid.pending_title_changed);
        parser.advance_until_terminated(&mut grid, b"\x1b]0;Live task\x07", &|| None);
        assert!(std::mem::take(&mut grid.pending_title_changed));
        assert_eq!(grid.title.as_deref(), Some("Live task"));
        parser.advance_until_terminated(&mut grid, b"\x1b[23;0t", &|| None);
        assert!(std::mem::take(&mut grid.pending_title_changed));
        assert_eq!(grid.title.as_deref(), Some("First task"));
        parser.advance_until_terminated(&mut grid, b"\x1b[23;0tordinary text", &|| None);
        assert!(!grid.pending_title_changed);
        assert!(grid.program_status.take_change().is_none());
        assert_eq!(grid.program_status.snapshot()[0].state, "working");
        parser.advance_until_terminated(&mut grid, b"\x1bc", &|| None);
        assert!(!grid.pending_title_changed);
        assert_eq!(grid.title.as_deref(), Some("First task"));
        assert!(grid.program_status.take_change().unwrap().is_empty());
    }

    #[test]
    fn application_prompt_blocks_preserve_working_until_foreground_ownership_changes() {
        let mut parser = ProgramStatusVteParser::new();
        let mut grid = grid();
        // omp's genuine turn-start order: Working, then transcript A/B/C/D.
        let input = b"\x1b]7501;state=working:app=omp\x1b\\\x1b]133;A\x07user prompt\x1b]133;B\x07\x1b]133;C\x07\x1b]133;D;0\x07";
        for byte in input {
            parser.advance_until_terminated(&mut grid, &[*byte], &|| Some(200));
        }
        assert_eq!(grid.program_status.take_change().unwrap()[0].state, "working");
        parser.advance_until_terminated(&mut grid, b"\x1b]133;A\x07", &|| None);
        assert!(grid.program_status.take_change().is_none());
        assert_eq!(grid.program_status.snapshot()[0].state, "working");
        parser.advance_until_terminated(&mut grid, b"\x1b]133;A\x07", &|| Some(100));
        assert!(grid.program_status.take_change().unwrap().is_empty());
    }

    #[test]
    fn same_chunk_prompt_expiry_does_not_erase_a_later_reporters_status() {
        let mut parser = ProgramStatusVteParser::new();
        let mut grid = grid();
        let groups = [200, 200, 100, 300, 300];
        let query = std::cell::Cell::new(0);
        let read_foreground = || {
            let index = query.get();
            query.set(index + 1);
            Some(groups[index])
        };
        parser.advance_until_terminated(
            &mut grid,
            b"\x1b]7501;state=working:app=first\x07\x1b]133;A\x07\x1b]133;A\x07\x1b]7501;state=working:app=second\x07\x1b]133;A\x07",
            &read_foreground,
        );
        assert_eq!(query.get(), groups.len());
        let snapshot = grid.program_status.take_change().unwrap();
        assert_eq!(snapshot[0].state, "working");
        assert_eq!(snapshot[0].app.as_deref(), Some("second"));
        parser.advance_until_terminated(&mut grid, b"\x1b]7501;state=clear\x07\x1b]133;A\x07", &|| panic!("clear must not query ownership"));
        assert!(grid.program_status.take_change().unwrap().is_empty());
    }

    #[test]
    fn prompt_expiry_preserves_unknown_ownership_and_nonrunning_records() {
        let mut parser = ProgramStatusVteParser::new();
        let mut grid = grid();
        for state in ["working", "blocked:kind=permission", "blocked:kind=question", "error", "done", "idle"] {
            parser.advance_until_terminated(&mut grid, format!("\x1b]7501;state={state}:app=test\x07\x1b]133;A\x07").as_bytes(), &|| None);
            assert_eq!(grid.program_status.snapshot()[0].state, state.split(':').next().unwrap());
            parser.advance_until_terminated(&mut grid, b"\x1bc", &|| panic!("reset must not query ownership"));
            assert!(grid.program_status.snapshot().is_empty());
        }
        for state in ["error", "done", "idle"] {
            parser.advance_until_terminated(&mut grid, format!("\x1b]7501;state={state}:app=test\x07").as_bytes(), &|| Some(200));
            parser.advance_until_terminated(&mut grid, b"\x1b]133;A\x07", &|| panic!("nonrunning prompt must not query ownership"));
            assert_eq!(grid.program_status.snapshot()[0].state, state);
        }
        for kind in ["permission", "question"] {
            parser.advance_until_terminated(&mut grid, format!("\x1b]7501;state=blocked:kind={kind}:app=test\x07\x1b]133;A\x07").as_bytes(), &|| Some(200));
            assert_eq!(grid.program_status.snapshot()[0].state, "blocked");
            assert_eq!(grid.program_status.snapshot()[0].kind.as_deref(), Some(kind));
            parser.advance_until_terminated(&mut grid, b"\x1b]133;A\x07", &|| Some(100));
            assert!(grid.program_status.snapshot().is_empty());
        }
    }

    #[test]
    fn child_only_reports_claim_foreground_without_overriding_a_known_root_owner() {
        let mut parser = ProgramStatusVteParser::new();
        let mut grid = grid();
        parser.advance_until_terminated(&mut grid, b"\x1b]7501;state=working:id=build:app=test\x07\x1b]133;A\x07", &|| Some(200));
        assert_eq!(grid.program_status.snapshot()[0].state, "working");
        parser.advance_until_terminated(&mut grid, b"\x1b]133;A\x07", &|| Some(100));
        assert!(grid.program_status.snapshot().is_empty());
        parser.advance_until_terminated(&mut grid, b"\x1b]7501;state=idle:app=root\x07", &|| Some(200));
        parser.advance_until_terminated(&mut grid, b"\x1b]7501;state=working:id=build\x07", &|| panic!("known root ownership must not be replaced by child report"));
        parser.advance_until_terminated(&mut grid, b"\x1b]133;A\x07", &|| Some(200));
        assert!(grid.program_status.snapshot().iter().any(|record| record.id == "build"));
        parser.advance_until_terminated(&mut grid, b"\x1b]133;A\x07", &|| Some(100));
        assert_eq!(grid.program_status.snapshot().len(), 1);
        assert_eq!(grid.program_status.snapshot()[0].state, "idle");
    }

    #[test]
    fn kitty_clipboard_raw_fragmentation_and_oversized_packets_are_bounded() {
        let mut parser = ProgramStatusVteParser::new();
        let mut grid = grid();
        let mut hostile = b"\x1b]5522;type=read;".to_vec();
        hostile.extend(std::iter::repeat_n(b'A', 128 * 1024));
        hostile.extend_from_slice(b"\x1b\\");
        for chunk in hostile.chunks(1031) {
            assert_eq!(parser.advance_until_terminated(&mut grid, chunk, &|| None), chunk.len());
        }
        assert!(grid.pending_forwarded_queries.is_empty());
        assert!(parser.body.capacity() <= 32768);
        let valid = b"\x1b]5522;type=read:id=pane;Lg==\x1b\\";
        for byte in valid {
            assert_eq!(parser.advance_until_terminated(&mut grid, &[*byte], &|| None), 1);
        }
        assert_eq!(grid.pending_forwarded_queries, vec![crate::host_query::HostQuery::KittyClipboard {
            bytes: valid.to_vec(), write: false,
        }]);
    }

    #[test]
    fn kitty_clipboard_raw_cancellation_does_not_turn_malformed_metadata_into_a_request() {
        let mut parser = ProgramStatusVteParser::new();
        let mut grid = grid();
        parser.advance_until_terminated(&mut grid, b"\x1b]5522;type=re\x01ad;Lg==\x1b\\", &|| None);
        assert!(grid.pending_forwarded_queries.is_empty());
        parser.advance_until_terminated(&mut grid, b"\x1b]5522;type=read;Lg==\x18", &|| None);
        assert!(grid.pending_forwarded_queries.is_empty());
        parser.advance_until_terminated(&mut grid, b"\x1b[?5522$p", &|| None);
        assert_eq!(grid.pending_forwarded_queries, vec![crate::host_query::HostQuery::KittyClipboardSupport]);
    }

    #[test]
    fn kitty_clipboard_rejected_write_packets_abort_instead_of_sending_truncated_contents() {
        let mut oversized = b"\x1b]5522;type=wdata:mime=dGV4dC9wbGFpbg==;".to_vec();
        oversized.extend(std::iter::repeat_n(b'A', 16384));
        oversized.extend_from_slice(b"\x1b\\");
        for (bad, status) in [
            (oversized, "EFBIG"),
            (b"\x1b]5522;type=wdata:id=x:id=y:mime=dGV4dC9wbGFpbg==;Q0ND\x1b\\".to_vec(), "EINVAL"),
            (b"\x1b]5522;type=wdata:mime=dGV4dC9wbGFpbg==;Q0ND\x18".to_vec(), "EINVAL"),
            (b"\x1b]5522;type=wdata:mime=dGV4dC9wbGFpbg==;Q0ND\x1ba".to_vec(), "EINVAL"),
        ] {
            let mut parser = ProgramStatusVteParser::new();
            let mut grid = grid();
            let stream = [
                b"\x1b]5522;type=write:id=writer\x1b\\".as_slice(),
                b"\x1b]5522;type=wdata:mime=dGV4dC9wbGFpbg==;QUFB\x1b\\",
                &bad,
                b"\x1b]5522;type=wdata:mime=dGV4dC9wbGFpbg==;QkJC\x1b\\",
                b"\x1b]5522;type=wdata\x1b\\",
            ].concat();
            for chunk in stream.chunks(509) {
                assert_eq!(parser.advance_until_terminated(&mut grid, chunk, &|| None), chunk.len());
            }
            assert!(grid.pending_forwarded_queries.is_empty(), "host must not receive the surviving AAABBB bytes");
            assert_eq!(grid.pending_messages_to_pty, vec![format!("\x1b]5522;type=write:status={}:id=writer\x1b\\", status).into_bytes()]);
            let replacement = b"\x1b]5522;type=write:id=new\x1b\\\
                \x1b]5522;type=wdata:mime=dGV4dC9wbGFpbg==;bmV3\x1b\\\
                \x1b]5522;type=wdata\x1b\\";
            assert_eq!(parser.advance_until_terminated(&mut grid, replacement, &|| None), replacement.len());
            assert_eq!(grid.pending_forwarded_queries, vec![crate::host_query::HostQuery::KittyClipboard {
                bytes: replacement.to_vec(), write: true,
            }]);
        }
    }

    #[test]
    fn kitty_clipboard_payload_controls_cannot_escape_into_the_host_terminal() {
        for control in [0, 0x01, 0x7f, 0x80, 0x9b, 0x9c, 0xff] {
            let mut parser = ProgramStatusVteParser::new();
            let mut grid = grid();
            let stream = [
                b"\x1b]5522;type=write:id=writer\x1b\\".as_slice(),
                b"\x1b]5522;type=wdata:mime=dGV4dC9wbGFpbg==;QUFB",
                &[control],
                b"QkJC\x1b\\\x1b]5522;type=wdata\x1b\\",
            ].concat();
            assert_eq!(parser.advance_until_terminated(&mut grid, &stream, &|| None), stream.len());
            assert!(grid.pending_forwarded_queries.is_empty(), "control {control:#x} must not reach the host");
            assert_eq!(grid.pending_messages_to_pty,
                vec![b"\x1b]5522;type=write:status=EINVAL:id=writer\x1b\\".to_vec()]);
        }
    }
}
