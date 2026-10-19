//! Continuous, parser for host-terminal replies arriving on
//! stdin.
//!
//! This parser routes stdin bytes through a private `termwiz::InputParser`,
//! classifies OSC / CSI-report events into `HostReply` variants, and lets
//! all other bytes (keyboard input) pass through as a residue byte sequence
//! that the caller feeds to the normal keyboard parser.

use lazy_static::lazy_static;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use zellij_utils::{
    data::HostTerminalThemeMode,
    ipc::PixelDimensions,
    nested_session,
    pane_size::SizeInPixels,
    vendored::termwiz::input::{InputEvent, InputParser},
};

/// Describe the terminal implementation of synchronised output
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum SyncOutput {
    DCS,
    CSI,
}

impl SyncOutput {
    pub fn start_seq(&self) -> &'static [u8] {
        static CSI_BSU_SEQ: &'static [u8] = "\u{1b}[?2026h".as_bytes();
        static DCS_BSU_SEQ: &'static [u8] = "\u{1b}P=1s\u{1b}".as_bytes();
        match self {
            SyncOutput::DCS => DCS_BSU_SEQ,
            SyncOutput::CSI => CSI_BSU_SEQ,
        }
    }

    pub fn end_seq(&self) -> &'static [u8] {
        static CSI_ESU_SEQ: &'static [u8] = "\u{1b}[?2026l".as_bytes();
        static DCS_ESU_SEQ: &'static [u8] = "\u{1b}P=2s\u{1b}".as_bytes();
        match self {
            SyncOutput::DCS => DCS_ESU_SEQ,
            SyncOutput::CSI => CSI_ESU_SEQ,
        }
    }
}

/// A classified host-terminal reply received on stdin.
///
/// The variants track the reply types Zellij consumes for its own
/// synchronous render hot-path (pixel dims, bg/fg, palette registers,
/// sync-output support). Accumulated forwarded-query byte streams take
/// a separate path: they ride `ParseOutput::completed_forward` through
/// a dedicated input-instruction channel so they don't get co-mingled
/// with semantically-typed state updates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HostReply {
    PixelDimensions(PixelDimensions),
    BackgroundColor(String),
    ForegroundColor(String),
    ColorRegisters(Vec<(usize, String)>),
    SynchronizedOutput(Option<SyncOutput>),
    /// DSR 997 reply / unsolicited notification reporting the host
    /// terminal's color-palette theme mode (CSI 2031).
    HostTerminalThemeChanged(HostTerminalThemeMode),
    KittyGraphicsSupport(bool),
    SixelSupport(bool),
}

/// Retained alias for the pre-refactor type name used by other modules in
/// the client. New code should prefer `HostReply`; the alias keeps the
/// existing `InputInstruction::AnsiStdinInstructions(Vec<...>)` plumbing
/// stable during the migration.
pub type AnsiStdinInstruction = HostReply;

impl HostReply {
    /// Classify an OSC payload (the bytes between the `ESC ]` prefix and
    /// the ST/BEL terminator) into a known `HostReply`, if possible.
    pub fn from_osc_payload(payload: &[u8]) -> Option<HostReply> {
        lazy_static! {
            // OSC 10 (foreground) / OSC 11 (background) answer form:
            //   OSC 10 ; <color> ST        e.g. "10;rgb:ffff/ffff/ffff"
            //   OSC 11 ; <color> ST
            static ref FG_RE: Regex = Regex::new(r"^10;(.*)$").unwrap();
            static ref BG_RE: Regex = Regex::new(r"^11;(.*)$").unwrap();
            // OSC 4 ; N ; <color> — palette-register answer.
            static ref COLOR_REGISTER_RE: Regex = Regex::new(r"^4;(\d+);(.*)$").unwrap();
        }
        let s = std::str::from_utf8(payload).ok()?;
        if let Some(caps) = BG_RE.captures(s) {
            return Some(HostReply::BackgroundColor(caps[1].to_string()));
        }
        if let Some(caps) = FG_RE.captures(s) {
            return Some(HostReply::ForegroundColor(caps[1].to_string()));
        }
        if let Some(caps) = COLOR_REGISTER_RE.captures(s) {
            let index: usize = caps[1].parse().ok()?;
            let color = caps[2].to_string();
            return Some(HostReply::ColorRegisters(vec![(index, color)]));
        }
        None
    }

    /// Classify a CSI-based report (the full raw sequence including the
    /// leading `ESC [`) into a `HostReply`, if possible.
    ///
    /// Recognised final bytes: `t` (pixel-dims reply, `CSI 4/6 ; H ; W t`),
    /// `y` (DECRPM reply to `CSI ?2026$p` — sync-output support
    /// advertisement).
    pub fn from_csi_report(raw: &[u8]) -> Option<HostReply> {
        let s = std::str::from_utf8(raw).ok()?;
        lazy_static! {
            // <ESC>[4;H;Wt or <ESC>[6;H;Wt
            static ref PIX_RE: Regex = Regex::new(r"^\u{1b}\[(\d+);(\d+);(\d+)t$").unwrap();
            // <ESC>[?2026;Ny — DECRPM reply for sync-output (VT mode 2026)
            static ref SYNC_RE: Regex = Regex::new(r"^\u{1b}\[\?2026;([0-4])\$y$").unwrap();
            // <ESC>[?997;1n (dark) / <ESC>[?997;2n (light) — DSR 997 reply
            // to CSI ?996n, or unsolicited host-theme notification when
            // CSI ?2031h is enabled.
            static ref THEME_RE: Regex = Regex::new(r"^\u{1b}\[\?997;([12])n$").unwrap();
        }
        if let Some(caps) = PIX_RE.captures(s) {
            let which: usize = caps[1].parse().ok()?;
            let first: usize = caps[2].parse().ok()?;
            let second: usize = caps[3].parse().ok()?;
            return match which {
                4 => Some(HostReply::PixelDimensions(PixelDimensions {
                    character_cell_size: None,
                    text_area_size: Some(SizeInPixels {
                        height: first,
                        width: second,
                    }),
                })),
                6 => Some(HostReply::PixelDimensions(PixelDimensions {
                    character_cell_size: Some(SizeInPixels {
                        height: first,
                        width: second,
                    }),
                    text_area_size: None,
                })),
                _ => None,
            };
        }
        if let Some(caps) = SYNC_RE.captures(s) {
            let code: usize = caps[1].parse().ok()?;
            return match code {
                1 | 2 | 3 => Some(HostReply::SynchronizedOutput(Some(SyncOutput::CSI))),
                _ => Some(HostReply::SynchronizedOutput(None)),
            };
        }
        if let Some(caps) = THEME_RE.captures(s) {
            let mode = match &caps[1] {
                "1" => HostTerminalThemeMode::Dark,
                "2" => HostTerminalThemeMode::Light,
                _ => return None,
            };
            return Some(HostReply::HostTerminalThemeChanged(mode));
        }
        None
    }

    /// Classify a Primary Device Attributes reply (`CSI ? Ps ; Ps ... c`)
    /// into a Sixel-capability advertisement. Attribute `4` in the reply
    /// means the host terminal supports Sixel graphics.
    ///
    /// Replies that are not a primary-DA form (eg. secondary-DA `CSI > ... c`)
    /// yield `None` so they do not clobber the host capability state.
    pub fn sixel_support_from_primary_da(raw: &[u8]) -> Option<HostReply> {
        lazy_static! {
            static ref PRIMARY_DA_RE: Regex = Regex::new(r"^\u{1b}\[\?([0-9;]*)c$").unwrap();
        }
        let s = std::str::from_utf8(raw).ok()?;
        let caps = PRIMARY_DA_RE.captures(s)?;
        let supports_sixel = caps[1].split(';').any(|attribute| attribute == "4");
        Some(HostReply::SixelSupport(supports_sixel))
    }
}

/// The "slot" tracking state for a single forwarded query currently in
/// flight to the host terminal. Replies belong to this slot only after
/// all earlier query batches have crossed their Primary-DA boundaries.
/// Its own Primary DA completes the captured bytes. The separate 500 ms
/// forward-timeout runtime owns the forwarding deadline.
#[derive(Debug, Clone)]
pub struct ForwardSlot {
    pub token: u32,
    pub reply_bytes: Vec<u8>,
    barriers_ahead: usize,
}

#[derive(Debug, Clone)]
pub struct ClipboardForwardSlot {
    pub token: u32,
    kitty: Option<KittyClipboardForward>,
}

#[derive(Debug, Clone)]
struct KittyClipboardForward {
    host_id: Vec<u8>,
    original_id: Option<Vec<u8>>,
    write: bool,
    started: bool,
    failed: bool,
    reply_bytes: Vec<u8>,
}

#[derive(Debug)]
struct ClipboardPasteSlot {
    context: u64,
    bytes: Vec<u8>,
    started: Instant,
}

pub const CLIENT_CLIPBOARD_FORWARD_TIMEOUT_MS: u64 = 30_000;

// A silent or unsupported host must not hold a real Escape key forever.
// Query replies otherwise keep their prefixes across keyboard idle ticks.
const HOST_REPLY_QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Return value of `feed()`.
#[derive(Debug, Clone, Default)]
pub struct ParseOutput {
    /// Classified host replies (zero or more).
    pub replies: Vec<HostReply>,
    /// A completed forwarded reply (Primary-DA barrier seen), ready to be
    /// sent to the server. At most one per feed call; more than one in a
    /// single feed would indicate the host emitted two barriers, in which
    /// case only the first is honored.
    pub completed_forward: Option<(u32, Vec<u8>)>,
    pub completed_clipboard_forward: Option<(u32, Vec<u8>)>,
    pub clipboard_pastes: Vec<(u64, Vec<u8>)>,
    /// OSC 99 notification-response payloads (one per OSC 99 found in
    /// the chunk). Routed by the caller as
    /// `InputInstruction::DesktopNotificationResponse`. Lives here, not
    /// in the keyboard-parser path, because the residue scrubber
    /// strips all OSC bytes before the keyboard parser sees them.
    pub desktop_notifications: Vec<Vec<u8>>,
    /// Residue bytes that were not classified as host replies. These are
    /// the bytes the caller should feed to the keyboard parser.
    pub residue: Vec<u8>,
    pub nested_frames: Vec<Vec<u8>>,
    /// `true` when the parser still holds buffered partial OSC/CSI
    /// bytes that need an idle-flush to release. The caller uses this
    /// to schedule a finalize tick even when the current chunk
    /// produced no residue (so a lone trailing ESC isn't stranded
    /// indefinitely). See `StdinAnsiParser::finalize`.
    pub has_partial_state: bool,
}

fn is_user_input(event: &InputEvent) -> bool {
    match event {
        InputEvent::Key(_) | InputEvent::Paste(_) => true,
        InputEvent::Mouse(mouse_event) => !mouse_event.mouse_buttons.is_empty(),
        InputEvent::PixelMouse(mouse_event) => !mouse_event.mouse_buttons.is_empty(),
        _ => false,
    }
}

/// Cap on the size of an in-flight partial OSC/CSI buffer. Sized to
/// pass legitimate OSC 52 clipboard payloads, which carry the entire
/// clipboard base64-encoded and have no protocol-level limit (images,
/// multi-MB text dumps, etc.). Beyond this we assume a runaway or
/// malformed sequence and flush the buffered bytes back to residue —
/// same observable behaviour as today for unterminated OSC, just
/// bounded.
const PARTIAL_BUFFER_CAP_BYTES: usize = 100 * 1024 * 1024;

const PASTE_START_MARKER: &[u8] = b"\x1b[200~";
const PASTE_END_MARKER: &[u8] = b"\x1b[201~";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingPartial {
    None,
    LoneEsc,
    BareIntroducer,
    ReplyInProgress,
    ExpectedReply,
}

impl PendingPartial {
    pub fn uses_reply_flush_guard(&self) -> bool {
        matches!(
            self,
            PendingPartial::ReplyInProgress | PendingPartial::ExpectedReply
        )
    }
}

#[derive(Debug, Clone, Copy)]
enum MarkerMatch {
    Complete,
    Prefix,
    No,
}

fn marker_match(buf: &[u8], marker: &[u8]) -> MarkerMatch {
    if buf.len() >= marker.len() {
        if &buf[..marker.len()] == marker {
            MarkerMatch::Complete
        } else {
            MarkerMatch::No
        }
    } else if marker.starts_with(buf) {
        MarkerMatch::Prefix
    } else {
        MarkerMatch::No
    }
}

/// Outcome of a single OSC/CSI walk over a byte buffer. Distinguishing
/// "needs more bytes" from "malformed" is what lets the residue
/// scrubber buffer partial sequences across `feed()` calls instead of
/// leaking their bytes into keyboard residue.
#[derive(Debug, Clone, Copy)]
enum SeqStatus {
    /// Sequence is complete; consume `len` bytes from the head of buf.
    Complete(usize),
    /// Sequence is a valid prefix; caller should buffer these bytes
    /// and prepend them to the next chunk.
    NeedMore,
    /// Sequence is malformed (bare ESC mid-payload, non-whitelisted
    /// final byte, length cap hit). Caller should fall through to
    /// emitting the leading byte as residue.
    Malformed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupKittyProbe {
    NotSent,
    AwaitingReply,
    Resolved,
}

/// Continuous host-reply parser. Lives for the whole client session.
pub struct StdinAnsiParser {
    inner: InputParser,
    /// Active forwarding slot: `Some` while a forwarded query is in
    /// flight, `None` otherwise.
    active_forward: Option<ForwardSlot>,
    active_clipboard_forward: Option<ClipboardForwardSlot>,
    clipboard_paste_context: Option<u64>,
    clipboard_paste_mode_initialized: bool,
    active_clipboard_paste: Option<ClipboardPasteSlot>,
    /// Bytes of an OSC sequence whose terminator hasn't arrived yet.
    /// Carried across feed() calls so the next chunk can complete it.
    partial_osc: Vec<u8>,
    partial_osc_clipboard_context: Option<Option<u64>>,
    partial_nested_clipboard_context: Option<Option<u64>>,
    discard_kitty_osc: bool,
    partial_kitty_deadline: Option<Instant>,
    /// Same for CSI device-control reports.
    partial_csi: Vec<u8>,
    partial_paste: Vec<u8>,
    nested_frame_extractor: nested_session::NestedFrameExtractor,
    in_bracketed_paste: bool,
    startup_kitty_probe: StartupKittyProbe,
    partial_apc: Vec<u8>,
    outstanding_host_query_barriers: usize,
    host_reply_deadline: Option<Instant>,
}

impl std::fmt::Debug for StdinAnsiParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StdinAnsiParser")
            .field("active_forward", &self.active_forward)
            .field("partial_osc_len", &self.partial_osc.len())
            .field("partial_csi_len", &self.partial_csi.len())
            .finish()
    }
}

impl StdinAnsiParser {
    pub fn new() -> Self {
        StdinAnsiParser {
            inner: InputParser::new(),
            active_forward: None,
            active_clipboard_forward: None,
            clipboard_paste_context: None,
            clipboard_paste_mode_initialized: false,
            active_clipboard_paste: None,
            partial_osc: Vec::new(),
            partial_osc_clipboard_context: None,
            partial_nested_clipboard_context: None,
            discard_kitty_osc: false,
            partial_kitty_deadline: None,
            partial_csi: Vec::new(),
            partial_paste: Vec::new(),
            nested_frame_extractor: nested_session::NestedFrameExtractor::new(),
            in_bracketed_paste: false,
            startup_kitty_probe: StartupKittyProbe::NotSent,
            partial_apc: Vec::new(),
            outstanding_host_query_barriers: 0,
            host_reply_deadline: None,
        }
    }

    pub fn expect_kitty_probe_reply(&mut self) {
        self.startup_kitty_probe = StartupKittyProbe::AwaitingReply;
    }

    /// Register a host-query batch before writing it to the terminal.
    /// Each batch must end with Primary DA so its replies have a boundary
    /// independent of stdin read sizes and keyboard Escape timeouts.
    pub fn expect_host_replies(&mut self) {
        if !self.host_replies_expected() {
            self.outstanding_host_query_barriers = 0;
        }
        self.outstanding_host_query_barriers += 1;
        self.host_reply_deadline = Some(Instant::now() + HOST_REPLY_QUERY_TIMEOUT);
    }

    fn host_replies_expected(&self) -> bool {
        self.outstanding_host_query_barriers > 0
            && self
                .host_reply_deadline
                .map(|deadline| Instant::now() < deadline)
                .unwrap_or(false)
    }

    fn complete_host_query(&mut self) {
        self.outstanding_host_query_barriers =
            self.outstanding_host_query_barriers.saturating_sub(1);
        if self.outstanding_host_query_barriers == 0 {
            self.host_reply_deadline = None;
        }
    }

    /// Open a forwarding window for `token`. After earlier query batches
    /// cross their DA boundaries, this batch's reply bytes are captured
    /// until its own DA, alongside normal classified `HostReply` events.
    ///
    /// The server serializes forwarded queries globally (`forward_in_flight`
    /// on `Screen`), but its own backstop timeout can release that slot and
    /// dispatch the next query before this client's per-slot timer has run,
    /// so callers hand a still-open slot over with `take_active_forward`
    /// first. The guards below catch a caller that skipped that handover:
    /// debug builds panic so bugs surface during testing, release builds
    /// log and clobber the previous slot (whose accumulated bytes would
    /// otherwise silently leak).
    pub fn open_forward(&mut self, token: u32) {
        debug_assert!(
            self.active_forward.is_none(),
            "open_forward({}) called while slot for token {:?} is still active",
            token,
            self.active_forward.as_ref().map(|s| s.token),
        );
        if let Some(existing) = self.active_forward.as_ref() {
            log::warn!(
                "open_forward({}) re-entered with existing slot token={} ({} accumulated bytes \
                 will be dropped); server serialization should have prevented this",
                token,
                existing.token,
                existing.reply_bytes.len(),
            );
        }
        self.expect_host_replies();
        self.active_forward = Some(ForwardSlot {
            token,
            reply_bytes: Vec::new(),
            barriers_ahead: self.outstanding_host_query_barriers - 1,
        });
    }

    /// Close an active forwarding window without a barrier (timeout path).
    /// Returns the accumulated reply bytes and the token, if any.
    pub fn close_forward_on_timeout(&mut self, token: u32) -> Option<(u32, Vec<u8>)> {
        match &self.active_forward {
            Some(slot) if slot.token == token => {
                let slot = self.active_forward.take().unwrap();
                Some((slot.token, slot.reply_bytes))
            },
            _ => None,
        }
    }

    /// Close whatever forwarding window is currently open, whichever token
    /// it belongs to, and return its token together with the bytes it
    /// accumulated. Used to hand a slot the server has already given up on
    /// over to the forward that replaced it, instead of clobbering it.
    pub fn take_active_forward(&mut self) -> Option<(u32, Vec<u8>)> {
        self.active_forward
            .take()
            .map(|slot| (slot.token, slot.reply_bytes))
    }

    pub fn open_clipboard_forward(&mut self, token: u32) {
        debug_assert!(
            self.active_clipboard_forward.is_none(),
            "open_clipboard_forward({}) called while slot for token {:?} is still active",
            token,
            self.active_clipboard_forward.as_ref().map(|s| s.token),
        );
        self.active_clipboard_forward = Some(ClipboardForwardSlot { token, kitty: None });
    }

    /// Register ownership before writing to stdout. Fresh, unguessable ids keep
    /// late replies (including replies from an earlier attachment) out of a new
    /// pane's transfer even when applications reuse the same id.
    pub fn prepare_clipboard_forward(&mut self, token: u32, query: Vec<u8>) -> Vec<u8> {
        use zellij_utils::kitty_clipboard::Packet;
        self.open_clipboard_forward(token);
        if !query.starts_with(b"\x1b]5522;") {
            return query;
        }
        let first_end = match osc_status(&query) {
            SeqStatus::Complete(end) => end,
            _ => return Vec::new(),
        };
        let first_term = if query[first_end - 1] == 7 { 1 } else { 2 };
        let packet = match Packet::parse(&query[2..first_end - first_term]) {
            Some(packet) => packet,
            None => return Vec::new(),
        };
        let host_id = uuid::Uuid::new_v4().simple().to_string().into_bytes();
        let original_id = packet.field(b"id").map(|_| packet.id());
        let write = packet.field(b"type") == Some(b"write");
        let mut outgoing = Vec::with_capacity(query.len());
        let mut remaining = query.as_slice();
        while !remaining.is_empty() {
            let end = match osc_status(remaining) {
                SeqStatus::Complete(end) => end,
                _ => return Vec::new(),
            };
            let term_len = if remaining[end - 1] == 7 { 1 } else { 2 };
            let packet = match Packet::parse(&remaining[2..end - term_len]) {
                Some(packet) => packet,
                None => return Vec::new(),
            };
            outgoing.extend_from_slice(&packet.with_id(Some(&host_id), &remaining[end - term_len..end]));
            remaining = &remaining[end..];
        }
        self.active_clipboard_forward.as_mut().unwrap().kitty = Some(KittyClipboardForward {
            host_id, original_id, write, started: false, failed: false, reply_bytes: Vec::new(),
        });
        outgoing
    }

    /// This context selects only future paste events. An already-started stream
    /// retains its original recipient even when the user changes pane/client.
    pub fn set_clipboard_paste_context(&mut self, context: Option<u64>) -> &'static [u8] {
        let changed = self.clipboard_paste_context.is_some() != context.is_some();
        self.clipboard_paste_context = context;
        if !changed && self.clipboard_paste_mode_initialized { return b""; }
        self.clipboard_paste_mode_initialized = true;
        if context.is_some() { b"\x1b[?5522h" } else { b"\x1b[?5522l" }
    }

    fn capture_clipboard_paste(
        &mut self,
        packet: &zellij_utils::kitty_clipboard::Packet<'_>,
        raw: &[u8],
        frame_context: Option<u64>,
        out: &mut ParseOutput,
    ) {
        use zellij_utils::kitty_clipboard::{paste_authorization, MAX_PASTE_BYTES};
        if packet.field(b"type") != Some(b"read") { return; }
        match packet.field(b"status") {
            Some(b"OK") => {
                self.active_clipboard_paste = frame_context.map(|context| ClipboardPasteSlot {
                    context, bytes: Vec::new(), started: Instant::now(),
                });
            },
            Some(b"DATA") if packet.field(b"mime") == Some(b"Lg==") => {},
            Some(b"DONE") => {},
            _ => { self.active_clipboard_paste = None; return; },
        }
        let slot = match self.active_clipboard_paste.as_mut() {
            Some(slot) => slot,
            None => return,
        };
        if slot.started.elapsed() >= Duration::from_secs(30)
            || slot.bytes.len().saturating_add(raw.len()) > MAX_PASTE_BYTES
        {
            self.active_clipboard_paste = None;
            return;
        }
        slot.bytes.extend_from_slice(raw);
        if packet.field(b"status") == Some(b"DONE") {
            let slot = self.active_clipboard_paste.take().unwrap();
            if paste_authorization(&slot.bytes).is_some() {
                out.clipboard_pastes.push((slot.context, slot.bytes));
            }
        }
    }

    fn capture_kitty_clipboard(&mut self, raw: &[u8], frame_context: Option<u64>, out: &mut ParseOutput) {
        use zellij_utils::kitty_clipboard::{Packet, MAX_TRANSFER_BYTES};
        let term_len = if raw.last() == Some(&7) { 1 } else { 2 };
        let packet = match Packet::parse(&raw[2..raw.len() - term_len]) {
            Some(packet) => packet,
            None => {
                self.reject_kitty_clipboard_packet(&raw[2..], out,
                    raw.len() - 2 - term_len > zellij_utils::kitty_clipboard::MAX_PACKET_BYTES);
                return;
            },
        };
        if packet.field(b"id").is_none() {
            if packet.field(b"type") == Some(b"read") && packet.field(b"status") == Some(b"DATA")
                && (self.active_clipboard_paste.is_none() || packet.field(b"mime") != Some(b"Lg=="))
            {
                // Actual clipboard contents without the request id cannot be a
                // paste listing. Dropping just this DATA could truncate a read.
                self.active_clipboard_paste = None;
                self.abort_kitty_clipboard_forward(out, false);
                return;
            }
            self.capture_clipboard_paste(&packet, raw, frame_context, out);
            return;
        }
        let slot = match self.active_clipboard_forward.as_mut() {
            Some(slot) => slot,
            None => return,
        };
        let kitty = match slot.kitty.as_mut() {
            Some(kitty) => kitty,
            None => return,
        };
        let kind: &[u8] = if kitty.write { b"write" } else { b"read" };
        if packet.field(b"id") != Some(kitty.host_id.as_slice()) || packet.field(b"type") != Some(kind) {
            return;
        }
        if kitty.failed {
            self.reject_kitty_clipboard_packet(&raw[2..], out, false);
            return;
        }
        let status = match packet.field(b"status") {
            Some(status) => status,
            None => {
                self.reject_kitty_clipboard_packet(&raw[2..], out, false);
                return;
            },
        };
        let complete = match status {
            b"OK" if !kitty.write && !kitty.started => { kitty.started = true; false },
            b"DATA" if !kitty.write && kitty.started && packet.field(b"mime").is_some() && packet.payload.is_some() => false,
            b"DONE" if kitty.write || kitty.started => true,
            b"ENOSYS" | b"EPERM" | b"EBUSY" => true,
            b"EIO" | b"EINVAL" | b"EFBIG" if kitty.write => true,
            _ => {
                self.reject_kitty_clipboard_packet(&raw[2..], out, false);
                return;
            },
        };
        let restored = packet.with_id(kitty.original_id.as_deref(), &raw[raw.len() - term_len..]);
        if kitty.reply_bytes.len().saturating_add(restored.len()) > MAX_TRANSFER_BYTES {
            // Never deliver a truncated binary object. The application can retry.
            let payload = if kitty.write { b"5522;type=write:status=EFBIG".as_slice() }
                else { b"5522;type=read:status=EBUSY".as_slice() };
            let reply = Packet::parse(payload).unwrap().with_id(kitty.original_id.as_deref(), b"\x1b\\");
            out.completed_clipboard_forward = Some((slot.token, reply));
            self.active_clipboard_forward = None;
            return;
        }
        kitty.reply_bytes.extend_from_slice(&restored);
        if complete {
            let slot = self.active_clipboard_forward.take().unwrap();
            out.completed_clipboard_forward = Some((slot.token, slot.kitty.unwrap().reply_bytes));
        }
    }

    fn reject_kitty_clipboard_packet(&mut self, payload: &[u8], out: &mut ParseOutput, oversized: bool) {
        use zellij_utils::kitty_clipboard::Packet;
        // The metadata fits before the first payload separator in normal DATA
        // packets, so an oversized stale generation need not cancel a new one.
        let metadata_end = payload.iter().enumerate().skip(5)
            .find_map(|(index, byte)| (*byte == b';' || *byte < 0x20 || *byte == 0x7f).then_some(index))
            .unwrap_or(payload.len());
        let metadata = Packet::parse(&payload[..metadata_end]);
        if let Some(packet) = metadata.as_ref().filter(|packet| packet.field(b"id").is_none()) {
            let unsolicited = packet.field(b"type") == Some(b"read")
                && (matches!(packet.field(b"status"), Some(b"OK") | Some(b"DONE"))
                    || (packet.field(b"status") == Some(b"DATA")
                        && packet.field(b"mime") == Some(b"Lg==")
                        && self.active_clipboard_paste.is_some()));
            self.active_clipboard_paste = None;
            if unsolicited { return; }
        }
        if metadata.is_none() { self.active_clipboard_paste = None; }
        if let Some(slot) = self.active_clipboard_forward.as_ref() {
            if let Some(kitty) = slot.kitty.as_ref() {
                if metadata.as_ref().and_then(|packet| packet.field(b"id"))
                    .map(|id| id != kitty.host_id.as_slice()).unwrap_or(false)
                {
                    return;
                }
            }
        }
        self.abort_kitty_clipboard_forward(out, oversized);
    }

    fn abort_kitty_clipboard_forward(&mut self, out: &mut ParseOutput, oversized: bool) {
        use zellij_utils::kitty_clipboard::Packet;
        if let Some(slot) = self.active_clipboard_forward.take() {
            if let Some(kitty) = slot.kitty {
                let payload = if !kitty.write { b"5522;type=read:status=EBUSY".as_slice() }
                    else if oversized { b"5522;type=write:status=EFBIG".as_slice() }
                    else { b"5522;type=write:status=EINVAL".as_slice() };
                let reply = Packet::parse(payload).unwrap()
                    .with_id(kitty.original_id.as_deref(), b"\x1b\\");
                out.completed_clipboard_forward = Some((slot.token, reply));
            } else {
                self.active_clipboard_forward = Some(slot);
            }
        }
    }

    pub fn close_clipboard_forward_on_timeout(&mut self, token: u32) -> Option<(u32, Vec<u8>)> {
        match &self.active_clipboard_forward {
            Some(slot) if slot.token == token => {
                let slot = self.active_clipboard_forward.take().unwrap();
                Some((slot.token, Vec::new()))
            },
            _ => None,
        }
    }

    pub fn take_active_clipboard_forward(&mut self) -> Option<(u32, Vec<u8>)> {
        let slot = self.active_clipboard_forward.take()?;
        Some((slot.token, Vec::new()))
    }

    #[cfg(test)]
    pub fn active_clipboard_forward_token(&self) -> Option<u32> {
        self.active_clipboard_forward.as_ref().map(|s| s.token)
    }

    /// Currently-open slot's token, if any. Test-only inspector;
    /// production code drives slot lifecycle through `open_forward`,
    /// `close_forward_on_timeout`, and `feed()` directly.
    #[cfg(test)]
    pub fn active_forward_token(&self) -> Option<u32> {
        self.active_forward.as_ref().map(|s| s.token)
    }

    /// Consume a chunk of raw stdin bytes. Returns classified host replies
    /// (to be dispatched to the server's cached-state consumers), at most
    /// one completed forwarded reply (barrier closed the window), and the
    /// residue bytes that were not part of any classified sequence — these
    /// are the bytes the caller should feed to the keyboard parser.
    pub fn feed(&mut self, bytes: &[u8]) -> ParseOutput {
        let mut out = ParseOutput::default();
        // The nested-frame extractor also buffers a lone ESC. Preserve its
        // recipient before the next read can identify it as an OSC introducer.
        let initial_context = if self.nested_frame_extractor.partial_bytes() == [0x1b]
            && bytes.first() == Some(&b']')
        {
            self.partial_nested_clipboard_context.take()
        } else {
            None
        };
        let had_new_bytes = !bytes.is_empty();
        let (bytes, nested_frames) = self.nested_frame_extractor.extract(bytes);
        if self.nested_frame_extractor.partial_bytes() == [0x1b] {
            if had_new_bytes {
                self.partial_nested_clipboard_context = Some(self.clipboard_paste_context);
            }
        } else {
            self.partial_nested_clipboard_context = None;
        }
        let bytes = &bytes[..];
        out.nested_frames = nested_frames;
        let sanitized = self.extract_kitty_probe_reply(bytes, &mut out.replies);
        // Frame before termwiz: OSC 5522 must retain exact payload/terminator
        // bytes without passing through its unbounded OSC buffer.
        let (residue, input_events) = self.strip_replies(&sanitized, initial_context, &mut out);
        let mut events = Vec::new();
        self.inner.parse(
            &input_events,
            |event| {
                events.push(event);
            },
            true, // maybe_more — typical stream usage
        );
        for event in events {
            match event {
                InputEvent::OperatingSystemCommand(payload) => {
                    // OSC 99 (desktop-notification response) is routed
                    // here rather than from the keyboard parser because
                    // the residue scrubber removes all OSC bytes before
                    // the keyboard parser runs. Other OSCs are
                    // classified into `HostReply` for cached-state
                    // refinement.
                    if payload.starts_with(b"99;") {
                        out.desktop_notifications
                            .push(payload.get(3..).unwrap_or_default().to_vec());
                        continue;
                    } else if let Some(reply) = HostReply::from_osc_payload(&payload) {
                        out.replies.push(reply);
                    }
                    if payload.starts_with(b"52;") {
                        if self.active_clipboard_forward.as_ref().map(|slot| slot.kitty.is_none()).unwrap_or(false) {
                            let slot = self.active_clipboard_forward.take().unwrap();
                            let mut bytes = b"\x1b]".to_vec();
                            bytes.extend_from_slice(&payload);
                            bytes.extend_from_slice(b"\x1b\\");
                            out.completed_clipboard_forward = Some((slot.token, bytes));
                        }
                        // Clipboard replies never belong to a color/size query,
                        // including late OSC52 replies during a Kitty transaction.
                        continue;
                    }
                    if let Some(slot) = self
                        .active_forward
                        .as_mut()
                        .filter(|slot| slot.barriers_ahead == 0)
                    {
                        // Re-serialize so the pane's pty sees a legal OSC.
                        // Terminators vary by host; ST (ESC \) is always safe.
                        slot.reply_bytes.extend_from_slice(b"\x1b]");
                        slot.reply_bytes.extend_from_slice(&payload);
                        slot.reply_bytes.extend_from_slice(b"\x1b\\");
                    }
                },
                InputEvent::DeviceControlReply {
                    params,
                    final_byte,
                    raw,
                    ..
                } => {
                    match final_byte {
                        b'c' => {
                            self.complete_host_query();
                            if self.startup_kitty_probe == StartupKittyProbe::AwaitingReply {
                                self.startup_kitty_probe = StartupKittyProbe::Resolved;
                                out.replies.push(HostReply::KittyGraphicsSupport(false));
                            }
                            if let Some(reply) = HostReply::sixel_support_from_primary_da(&raw) {
                                out.replies.push(reply);
                            }
                            // A local or timed-out earlier query's DA
                            // cannot complete this forwarding window.
                            match self.active_forward.as_mut() {
                                Some(slot) if slot.barriers_ahead > 0 => {
                                    slot.barriers_ahead -= 1;
                                },
                                Some(_) => {
                                    let slot = self.active_forward.take().unwrap();
                                    out.completed_forward = Some((slot.token, slot.reply_bytes));
                                },
                                None => {},
                            }
                            // Primary-DA is NOT double-dispatched — it has
                            // no cached-state counterpart.
                        },
                        _ => {
                            if let Some(reply) = HostReply::from_csi_report(&raw) {
                                out.replies.push(reply);
                            }
                            if let Some(slot) = self
                                .active_forward
                                .as_mut()
                                .filter(|slot| slot.barriers_ahead == 0)
                            {
                                slot.reply_bytes.extend_from_slice(&raw);
                            }
                            // Suppress unused-variable warning for params.
                            let _ = params;
                        },
                    }
                },
                // Everything else is keyboard / mouse / paste / wake input;
                // we need those bytes to reach the keyboard parser. We can
                // not reconstruct the exact bytes from a parsed event here,
                // so we rely on the caller's own second pass through the
                // keyboard parser: the residue is the concatenation of all
                // input bytes that are NOT part of a classified reply. To
                // produce that residue deterministically, we re-scan the
                // buffer a second time below.
                other => {
                    if self.active_clipboard_forward.as_ref().map(|slot| slot.kitty.is_none()).unwrap_or(false)
                        && is_user_input(&other)
                        && out.completed_clipboard_forward.is_none()
                    {
                        if let Some(slot) = self.active_clipboard_forward.take() {
                            out.completed_clipboard_forward = Some((slot.token, Vec::new()));
                        }
                    }
                },
            }
        }
        out.residue = residue;
        out.has_partial_state = !self.partial_osc.is_empty()
            || self.discard_kitty_osc
            || !self.partial_csi.is_empty()
            || !self.partial_paste.is_empty()
            || !self.nested_frame_extractor.partial_bytes().is_empty()
            || !self.partial_apc.is_empty();
        out
    }

    pub fn pending_partial(&self) -> PendingPartial {
        if is_kitty_clipboard_prefix(&self.partial_osc)
            && (self.active_clipboard_forward.as_ref().map(|slot| slot.kitty.is_some()).unwrap_or(false)
                || ((self.clipboard_paste_context.is_some() || self.active_clipboard_paste.is_some())
                    && self.partial_kitty_deadline.map(|deadline| Instant::now() < deadline).unwrap_or(false)))
            || self.host_replies_expected()
            && (!self.partial_osc.is_empty()
                || !self.partial_csi.is_empty()
                || !self.partial_apc.is_empty()
                || !self.nested_frame_extractor.partial_bytes().is_empty())
        {
            PendingPartial::ExpectedReply
        } else if self.discard_kitty_osc || !self.partial_paste.is_empty() || !self.partial_apc.is_empty() {
            PendingPartial::ReplyInProgress
        } else if self.partial_csi.is_empty()
            && self.partial_osc.is_empty()
            && self.nested_frame_extractor.partial_bytes() == [0x1b]
        {
            PendingPartial::LoneEsc
        } else if !self.nested_frame_extractor.partial_bytes().is_empty() {
            PendingPartial::ReplyInProgress
        } else if self.partial_csi.is_empty() && self.partial_osc.is_empty() {
            PendingPartial::None
        } else if self.partial_csi.is_empty() && self.partial_osc == [0x1b] {
            PendingPartial::LoneEsc
        } else if self.partial_csi.is_empty() && self.partial_osc == [0x1b, b']'] {
            PendingPartial::BareIntroducer
        } else if self.partial_osc.is_empty() && self.partial_csi == [0x1b, b'['] {
            PendingPartial::BareIntroducer
        } else {
            PendingPartial::ReplyInProgress
        }
    }

    pub fn finalize_fast_partial(&mut self) -> Vec<u8> {
        match self.pending_partial() {
            PendingPartial::LoneEsc | PendingPartial::BareIntroducer => {
                if !self.nested_frame_extractor.partial_bytes().is_empty() {
                    self.nested_frame_extractor.take_partial()
                } else if !self.partial_osc.is_empty() {
                    std::mem::take(&mut self.partial_osc)
                } else {
                    std::mem::take(&mut self.partial_csi)
                }
            },
            PendingPartial::None
            | PendingPartial::ReplyInProgress
            | PendingPartial::ExpectedReply => Vec::new(),
        }
    }

    pub fn finalize_force(&mut self) -> Vec<u8> {
        self.in_bracketed_paste = false;
        if self.partial_osc.starts_with(b"\x1b]5522;")
            || (self.partial_kitty_deadline.is_some() && is_kitty_clipboard_prefix(&self.partial_osc))
        {
            // A transaction deadline does not terminate its OSC on the wire.
            // Keep consuming the frame after idle ticks, but retain no payload.
            let trailing_escape = self.partial_osc.last() == Some(&0x1b);
            self.partial_osc.clear();
            if trailing_escape { self.partial_osc.push(0x1b); }
            self.discard_kitty_osc = true;
            self.partial_kitty_deadline = None;
            self.active_clipboard_paste = None;
            if let Some(kitty) = self.active_clipboard_forward.as_mut().and_then(|slot| slot.kitty.as_mut()) {
                kitty.failed = true;
                kitty.reply_bytes.clear();
            }
        }
        let mut partial_nested_frame = self.nested_frame_extractor.take_partial();
        let mut out = Vec::with_capacity(
            self.partial_osc.len()
                + self.partial_csi.len()
                + self.partial_paste.len()
                + partial_nested_frame.len()
                + self.partial_apc.len(),
        );
        if !self.discard_kitty_osc { out.append(&mut self.partial_osc); }
        out.append(&mut self.partial_csi);
        out.append(&mut self.partial_paste);
        out.append(&mut partial_nested_frame);
        out.append(&mut self.partial_apc);
        out
    }

    fn extract_kitty_probe_reply(&mut self, bytes: &[u8], replies: &mut Vec<HostReply>) -> Vec<u8> {
        let mut working = std::mem::take(&mut self.partial_apc);
        if working.is_empty() && self.partial_osc == [0x1b] && bytes.first() == Some(&b'_') {
            working.append(&mut self.partial_osc);
        }
        working.extend_from_slice(bytes);
        let mut out = Vec::with_capacity(working.len());
        let mut i = 0;
        while i < working.len() {
            let rest = &working[i..];
            if rest.len() >= 2 && rest[0] == 0x1b && rest[1] == b'_' {
                match rest.get(2) {
                    Some(&b'G') => {},
                    Some(_) => {
                        out.push(working[i]);
                        i += 1;
                        continue;
                    },
                    None => {
                        self.partial_apc = rest.to_vec();
                        return out;
                    },
                }
                match apc_status(rest) {
                    SeqStatus::Complete(len) => {
                        let payload = &rest[2..len - 2];
                        if self.startup_kitty_probe == StartupKittyProbe::AwaitingReply
                            && payload.first() == Some(&b'G')
                            && contains_subslice(payload, b"i=31")
                        {
                            replies.push(HostReply::KittyGraphicsSupport(contains_subslice(
                                payload, b"i=31;OK",
                            )));
                            self.startup_kitty_probe = StartupKittyProbe::Resolved;
                        }
                        i += len;
                        continue;
                    },
                    SeqStatus::NeedMore => {
                        let tail = rest.to_vec();
                        if tail.len() > PARTIAL_BUFFER_CAP_BYTES {
                            out.extend_from_slice(&tail);
                        } else {
                            self.partial_apc = tail;
                        }
                        return out;
                    },
                    SeqStatus::Malformed => {
                        out.push(working[i]);
                        i += 1;
                        continue;
                    },
                }
            }
            out.push(working[i]);
            i += 1;
        }
        out
    }

    /// Walk `bytes` (with any pending partial buffer prepended) and drop
    /// any OSC/whitelisted-CSI sequences, returning the remaining bytes
    /// verbatim (keyboard residue). This is a byte-level scrubber — it
    /// does not produce events, only bytes.
    ///
    /// If the chunk ends mid-sequence, the unterminated tail is held in
    /// `self.partial_osc` or `self.partial_csi` and prepended to the
    /// next call's input — so the corresponding bytes never reach
    /// residue (and never appear as spurious keypresses) while waiting
    /// for the rest of the sequence.
    fn strip_replies(&mut self, bytes: &[u8], initial_context: Option<Option<u64>>, parsed: &mut ParseOutput) -> (Vec<u8>, Vec<u8>) {
        // Prepend any pending partial. At most one of (partial_osc,
        // partial_csi) is non-empty at any time — the previous walk
        // either completed all sequences or stopped at exactly one
        // unterminated tail.
        let had_partial_osc = !self.partial_osc.is_empty();
        let carried_context = self.partial_osc_clipboard_context.take();
        let input_offset = self.partial_osc.len() + self.partial_csi.len() + self.partial_paste.len();
        let mut working: Vec<u8> = Vec::with_capacity(
            self.partial_osc.len()
                + self.partial_csi.len()
                + self.partial_paste.len()
                + bytes.len(),
        );
        working.append(&mut self.partial_osc);
        working.append(&mut self.partial_csi);
        working.append(&mut self.partial_paste);
        working.extend_from_slice(bytes);

        let mut out = Vec::with_capacity(working.len());
        let mut events = Vec::with_capacity(working.len());
        let mut i = 0;
        while i < working.len() {
            let rest = &working[i..];
            let frame_context = if i == 0 && had_partial_osc {
                carried_context.unwrap_or(self.clipboard_paste_context)
            } else if i == input_offset {
                initial_context.unwrap_or(self.clipboard_paste_context)
            } else {
                self.clipboard_paste_context
            };
            if self.discard_kitty_osc {
                if matches!(rest[0], 7 | 0x18 | 0x1a) {
                    self.discard_kitty_osc = false;
                    i += 1;
                } else if rest[0] == 0x1b {
                    match rest.get(1) {
                        Some(b'\\') => { self.discard_kitty_osc = false; i += 2; },
                        Some(_) => { self.discard_kitty_osc = false; },
                        None => {
                            self.partial_osc.push(0x1b);
                            self.partial_osc_clipboard_context = Some(frame_context);
                            return (out, events);
                        },
                    }
                } else {
                    i += 1;
                }
                continue;
            }
            if self.in_bracketed_paste {
                if rest[0] == 0x1b {
                    match marker_match(rest, PASTE_END_MARKER) {
                        MarkerMatch::Complete => {
                            out.extend_from_slice(PASTE_END_MARKER);
                            events.extend_from_slice(PASTE_END_MARKER);
                            self.in_bracketed_paste = false;
                            i += PASTE_END_MARKER.len();
                            continue;
                        },
                        MarkerMatch::Prefix => {
                            let tail = rest.to_vec();
                            if tail.len() > PARTIAL_BUFFER_CAP_BYTES {
                                out.extend_from_slice(&tail);
                            } else {
                                self.partial_paste = tail;
                            }
                            return (out, events);
                        },
                        MarkerMatch::No => {},
                    }
                }
                out.push(working[i]);
                events.push(working[i]);
                i += 1;
                continue;
            }
            if rest.len() >= 2 && rest[0] == 0x1b && rest[1] == b'[' {
                if let MarkerMatch::Complete = marker_match(rest, PASTE_START_MARKER) {
                    out.extend_from_slice(PASTE_START_MARKER);
                    events.extend_from_slice(PASTE_START_MARKER);
                    self.in_bracketed_paste = true;
                    i += PASTE_START_MARKER.len();
                    continue;
                }
            }
            // OSC: ESC ] ... (BEL | ESC \)
            if rest.len() >= 2 && rest[0] == 0x1b && rest[1] == b']' {
                match osc_status(rest) {
                    SeqStatus::Complete(len) => {
                        self.partial_kitty_deadline = None;
                        if rest.starts_with(b"\x1b]5522;") {
                            if matches!(rest[len - 1], 0x18 | 0x1a) {
                                self.reject_kitty_clipboard_packet(&rest[2..len - 1], parsed, false);
                            } else {
                                self.capture_kitty_clipboard(&rest[..len], frame_context, parsed);
                            }
                        } else {
                            events.extend_from_slice(&rest[..len]);
                        }
                        i += len;
                        continue;
                    },
                    SeqStatus::NeedMore => {
                        let kitty_prefix = is_kitty_clipboard_prefix(rest);
                        let expected = self.active_clipboard_forward.as_ref()
                            .map(|slot| slot.kitty.is_some()).unwrap_or(false)
                            || self.clipboard_paste_context.is_some()
                            || self.active_clipboard_paste.is_some();
                        if kitty_prefix && (rest.starts_with(b"\x1b]5522;")
                            || expected || self.partial_kitty_deadline.is_some())
                        {
                            self.partial_kitty_deadline.get_or_insert_with(|| Instant::now() + Duration::from_secs(30));
                        } else {
                            self.partial_kitty_deadline = None;
                        }
                        let tail = rest.to_vec();
                        let cap = if rest.starts_with(b"\x1b]5522;") {
                            zellij_utils::kitty_clipboard::MAX_PACKET_BYTES + 4
                        } else {
                            PARTIAL_BUFFER_CAP_BYTES
                        };
                        if tail.len() > cap && rest.starts_with(b"\x1b]5522;") {
                            // Host data must never become keyboard input after overflow.
                            self.reject_kitty_clipboard_packet(&rest[2..], parsed, true);
                            self.discard_kitty_osc = true;
                            self.partial_kitty_deadline = None;
                            if tail.last() == Some(&0x1b) { self.partial_osc.push(0x1b); }
                        } else if tail.len() > cap {
                            out.extend_from_slice(&tail);
                        } else {
                            self.partial_osc = tail;
                            self.partial_osc_clipboard_context = Some(frame_context);
                        }
                        return (out, events);
                    },
                    SeqStatus::Malformed => {
                        let next_escape = rest[2..].iter().position(|byte| *byte == 0x1b).map(|index| index + 2);
                        let kitty_prefix = rest.starts_with(b"\x1b]5522;")
                            || (self.partial_kitty_deadline.is_some()
                                && next_escape.map(|end| is_kitty_clipboard_prefix(&rest[..end])).unwrap_or(false));
                        self.partial_kitty_deadline = None;
                        if kitty_prefix {
                            self.reject_kitty_clipboard_packet(&rest[2..], parsed, false);
                            // An unexpected ESC ends the malformed clipboard packet,
                            // but the new escape sequence still belongs to input.
                            if let Some(next) = next_escape {
                                i += next;
                                continue;
                            }
                        }
                        out.push(working[i]);
                        events.push(working[i]);
                        i += 1;
                        continue;
                    },
                }
            }
            // Whitelisted CSI report: ESC [ <params>* <intermediates>* <final>
            if rest.len() >= 2 && rest[0] == 0x1b && rest[1] == b'[' {
                match csi_status(rest) {
                    SeqStatus::Complete(len) => {
                        events.extend_from_slice(&rest[..len]);
                        i += len;
                        continue;
                    },
                    SeqStatus::NeedMore => {
                        let tail = rest.to_vec();
                        if tail.len() > PARTIAL_BUFFER_CAP_BYTES {
                            out.extend_from_slice(&tail);
                        } else {
                            self.partial_csi = tail;
                        }
                        return (out, events);
                    },
                    SeqStatus::Malformed => {
                        out.push(working[i]);
                        events.push(working[i]);
                        i += 1;
                        continue;
                    },
                }
            }
            // Lone trailing ESC at the tail — could be the start of
            // either OSC or CSI; the next byte will disambiguate. Buffer
            // it under partial_osc by convention; the next call's
            // walker re-routes based on the actual second byte.
            if rest.len() == 1 && rest[0] == 0x1b {
                self.partial_osc = vec![0x1b];
                self.partial_osc_clipboard_context = Some(frame_context);
                return (out, events);
            }
            out.push(working[i]);
            events.push(working[i]);
            i += 1;
        }
        (out, events)
    }
}

/// Bare ESC / ESC ] remain keyboard ambiguities. Once the OSC number starts,
/// retain its clipboard association even if idle or query timeout splits it.
fn is_kitty_clipboard_prefix(bytes: &[u8]) -> bool {
    const PREFIX: &[u8] = b"\x1b]5522;";
    bytes.starts_with(PREFIX) || (bytes.len() >= 3 && PREFIX.starts_with(bytes))
}

/// Walk an OSC sequence starting at the head of `buf`. Returns whether
/// the sequence is complete, needs more bytes, or is malformed.
fn osc_status(buf: &[u8]) -> SeqStatus {
    if buf.get(0) != Some(&0x1b) || buf.get(1) != Some(&b']') {
        return SeqStatus::Malformed;
    }
    let mut i = 2;
    while i < buf.len() {
        match buf[i] {
            0x07 | 0x18 | 0x1a => return SeqStatus::Complete(i + 1),
            0x1b => match buf.get(i + 1) {
                Some(&b'\\') => return SeqStatus::Complete(i + 2),
                // Bare ESC followed by something other than `\` —
                // malformed under the ST-only termination we accept.
                Some(_) => return SeqStatus::Malformed,
                // ESC at the very tail; the next chunk may bring `\`
                // and finish the sequence.
                None => return SeqStatus::NeedMore,
            },
            _ => i += 1,
        }
    }
    SeqStatus::NeedMore
}

fn apc_status(buf: &[u8]) -> SeqStatus {
    if buf.get(0) != Some(&0x1b) || buf.get(1) != Some(&b'_') {
        return SeqStatus::Malformed;
    }
    let mut i = 2;
    while i < buf.len() {
        match buf[i] {
            0x1b => match buf.get(i + 1) {
                Some(&b'\\') => return SeqStatus::Complete(i + 2),
                Some(_) => return SeqStatus::Malformed,
                None => return SeqStatus::NeedMore,
            },
            _ => i += 1,
        }
    }
    SeqStatus::NeedMore
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Walk a whitelisted CSI report starting at the head of `buf`.
fn csi_status(buf: &[u8]) -> SeqStatus {
    if buf.get(0) != Some(&0x1b) || buf.get(1) != Some(&b'[') {
        return SeqStatus::Malformed;
    }
    let mut i = 2;
    let max = 256;
    while i < buf.len() && i < max {
        let b = buf[i];
        match b {
            0x30..=0x3F | 0x20..=0x2F => i += 1,
            b't' | b'y' | b'c' | b'n' => return SeqStatus::Complete(i + 1),
            0x40..=0x7E => return SeqStatus::Malformed, // non-whitelisted final
            _ => return SeqStatus::Malformed,
        }
    }
    if i >= max {
        // CSI ran past the cap without terminating — treat as malformed
        // so the leading byte falls through to residue and parsing
        // resumes from the next position.
        SeqStatus::Malformed
    } else {
        SeqStatus::NeedMore
    }
}

// =====================================================================
// Forward-slot timeout infrastructure
// =====================================================================

use std::sync::{Arc, Mutex, OnceLock};

/// Dedicated, lazily-initialised runtime for driving forward-slot
/// timeouts. A single current-thread executor runs on its own OS
/// thread; timer tasks are `spawn`-ed onto it from the synchronous
/// `ClientInstruction::ForwardQueryToHost` handler. One-thread model
/// because timer tasks do no CPU work — they just sleep and perform a
/// millisecond-scale mutex check on wake-up.
static FORWARD_TIMEOUT_RUNTIME: OnceLock<Arc<tokio::runtime::Runtime>> = OnceLock::new();

pub fn forward_timeout_runtime() -> &'static Arc<tokio::runtime::Runtime> {
    FORWARD_TIMEOUT_RUNTIME.get_or_init(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("failed to build forward-timeout runtime");
        let rt = Arc::new(rt);
        let rt_for_driver = rt.clone();
        // `block_on(pending())` keeps the executor loop alive forever
        // on this thread; spawned timer tasks are polled as they
        // become ready (on spawn, on wake from the time driver).
        std::thread::Builder::new()
            .name("zellij-client-forward-timeout".into())
            .spawn(move || {
                rt_for_driver.block_on(std::future::pending::<()>());
            })
            .expect("failed to spawn forward-timeout driver thread");
        rt
    })
}

/// Spawn a timer task that closes a forward slot after `deadline` and
/// invokes `on_timeout(token, reply_bytes)` with whatever the slot
/// accumulated. Token-guard idempotent: if the barrier (or a
/// replacement forward) has already cleared the slot by the time the
/// timer wakes, `close_forward_on_timeout(token)` returns `None` and
/// `on_timeout` is never called — no explicit cancellation path
/// required.
///
/// Extracted as a free function so tests can drive it against a
/// `tokio::time::pause()`-backed paused runtime without instantiating
/// the full client.
pub fn schedule_forward_timeout<F>(
    runtime: &tokio::runtime::Handle,
    parser: Arc<Mutex<StdinAnsiParser>>,
    token: u32,
    deadline: std::time::Duration,
    on_timeout: F,
) where
    F: FnOnce(u32, Vec<u8>) + Send + 'static,
{
    runtime.spawn(async move {
        tokio::time::sleep(deadline).await;
        let payload = parser.lock().unwrap().close_forward_on_timeout(token);
        if let Some((t, bytes)) = payload {
            on_timeout(t, bytes);
        }
    });
}

pub fn schedule_clipboard_forward_timeout<F>(
    runtime: &tokio::runtime::Handle,
    parser: Arc<Mutex<StdinAnsiParser>>,
    token: u32,
    deadline: std::time::Duration,
    on_timeout: F,
) where
    F: FnOnce(u32, Vec<u8>) + Send + 'static,
{
    runtime.spawn(async move {
        tokio::time::sleep(deadline).await;
        let payload = parser
            .lock()
            .unwrap()
            .close_clipboard_forward_on_timeout(token);
        if let Some((t, bytes)) = payload {
            on_timeout(t, bytes);
        }
    });
}

#[cfg(test)]
#[path = "stdin_ansi_parser_tests.rs"]
mod tests;
