//! Bound raw OSC 7501 before vte's general-purpose (unbounded) OSC buffer.
//! Other terminal sequences still go through vte, unchanged.
use super::grid::Grid;
use vte::Perform;

#[derive(Clone, Copy)]
enum Mode {
    Ground,
    Escape,
    Prefix(usize),
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

pub struct ProgramStatusVteParser {
    parser: vte::Parser,
    mode: Mode,
    body: Vec<u8>,
}

impl ProgramStatusVteParser {
    pub fn new() -> Self {
        Self { parser: vte::Parser::new(), mode: Mode::Ground, body: Vec::new() }
    }

    pub fn advance_until_terminated(&mut self, grid: &mut Grid, bytes: &[u8]) -> usize {
        let mut index = 0;
        while index < bytes.len() {
            // Ordinary grid text remains a single bulk parser call, not per-byte dispatch.
            if matches!(self.mode, Mode::Ground) && bytes[index] != 0x1b {
                let end = bytes[index..].iter().position(|b| *b == 0x1b).map_or(bytes.len(), |n| index + n);
                let consumed = self.parser.advance_until_terminated(grid, &bytes[index..end]);
                index += consumed;
                if index < end || !grid.pending_forwarded_queries.is_empty() { return index; }
                continue;
            }
            if let Mode::OtherString { bell, escape: false } = self.mode {
                if !matches!(bytes[index], 0x1b | 0x18 | 0x1a) && (!bell || bytes[index] != 7) {
                    let end = bytes[index..].iter().position(|b| matches!(*b, 0x1b | 0x18 | 0x1a) || (bell && *b == 7)).map_or(bytes.len(), |n| index + n);
                    let consumed = self.parser.advance_until_terminated(grid, &bytes[index..end]);
                    index += consumed;
                    if index < end || !grid.pending_forwarded_queries.is_empty() { return index; }
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
                    if byte == b"7501;"[n] {
                        if n == 4 {
                            // The already-forwarded prefix cannot dispatch anything; cancel it.
                            self.parser.advance(grid, b"\x18");
                            self.body.clear();
                            self.mode = Mode::Status { escape: false, discarded: false };
                            forward = false;
                        } else { self.mode = Mode::Prefix(n + 1); }
                    } else {
                        self.mode = if matches!(byte, 7 | 0x18 | 0x1a) { Mode::Ground }
                            else { Mode::OtherString { bell: true, escape: byte == 0x1b } };
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
                            grid.osc_dispatch(&[b"7501", &self.body], byte == 7);
                        }
                        self.body.clear();
                        self.mode = Mode::Ground;
                    } else if escape {
                        // An unexpected ESC starts another terminal sequence, not a valid ST.
                        self.body.clear();
                        self.parser.advance(grid, b"\x1b");
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
                if self.parser.advance_until_terminated(grid, &bytes[index..index + 1]) == 0 {
                    self.mode = previous;
                    return index;
                }
            }
            index += 1;
            if !grid.pending_forwarded_queries.is_empty() { return index; }
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
            assert_eq!(parser.advance_until_terminated(&mut grid, &[*byte]), 1);
        }
        assert_eq!(grid.pending_messages_to_pty.remove(0), b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x1b\\");
        parser.advance_until_terminated(&mut grid, b"\x1b[?1049h\x1b[!p\x1b[?1049l");
        assert_eq!(grid.program_status.snapshot()[0].state, "done");
        parser.advance_until_terminated(&mut grid, b"\x1b]7501;state=blocked:id=worker\x1b\\\x1b]133;A\x07");
        assert_eq!(grid.program_status.snapshot().iter().map(|r| r.state.as_str()).collect::<Vec<_>>(), vec!["done"]);
        parser.advance_until_terminated(&mut grid, b"\x1bc");
        assert!(grid.program_status.snapshot().is_empty());
    }

    #[test]
    fn raw_reports_are_bounded_atomic_and_do_not_disrupt_following_terminal_sequences() {
        let mut parser = ProgramStatusVteParser::new();
        let mut grid = grid();
        parser.advance_until_terminated(&mut grid, b"\x1b]7501;state=done:app=cargo\x07");
        let mut hostile = b"\x1b]7501;state=clear:unknown=".to_vec();
        hostile.extend(std::iter::repeat_n(b'a', 65536));
        hostile.extend_from_slice(b"\x1b\\\x1b]2;Ordinary window title\x07");
        assert_eq!(parser.advance_until_terminated(&mut grid, &hostile), hostile.len());
        assert_eq!(grid.program_status.snapshot()[0].state, "done");
        assert!(parser.body.capacity() <= 8192);
        // Vte would strip raw controls from an OSC. The raw bridge must not turn a bad state pair into a valid one.
        parser.advance_until_terminated(&mut grid, b"\x1b]7501;state=cl\x01ear\x07");
        assert_eq!(grid.program_status.snapshot()[0].state, "done");
        parser.advance_until_terminated(&mut grid, b"\x1b]7501;bad=\xff:state=working\x07");
        assert_eq!(grid.program_status.snapshot()[0].state, "working");
        parser.advance_until_terminated(&mut grid, b"\x1b]7501;state=clear\x1b\\");
        assert!(grid.program_status.snapshot().is_empty());
        parser.advance_until_terminated(&mut grid, b"\x1b]7501;state=done\x1b\\");
        assert_eq!(grid.program_status.snapshot()[0].state, "done");
    }
}
