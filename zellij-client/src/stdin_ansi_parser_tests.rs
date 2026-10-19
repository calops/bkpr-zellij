//! Unit tests for the continuous host-reply parser.

use super::{schedule_forward_timeout, HostReply, PendingPartial, StdinAnsiParser};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Helper: collect replies and residue from a single `feed` call.
fn feed_once(parser: &mut StdinAnsiParser, bytes: &[u8]) -> (Vec<HostReply>, Vec<u8>) {
    let out = parser.feed(bytes);
    (out.replies, out.residue)
}

#[test]
fn pixel_dimensions_text_area_reply() {
    // CSI 4 ; H ; W t
    let mut parser = StdinAnsiParser::new();
    let (replies, residue) = feed_once(&mut parser, b"\x1b[4;720;1280t");
    assert!(
        residue.is_empty(),
        "pixel-dim reply should be fully consumed"
    );
    assert_eq!(replies.len(), 1);
    match &replies[0] {
        HostReply::PixelDimensions(pd) => {
            let tas = pd.text_area_size.expect("text area size");
            assert_eq!(tas.height, 720);
            assert_eq!(tas.width, 1280);
            assert!(pd.character_cell_size.is_none());
        },
        other => panic!("unexpected reply: {:?}", other),
    }
}

#[test]
fn pixel_dimensions_character_cell_reply() {
    let mut parser = StdinAnsiParser::new();
    let (replies, residue) = feed_once(&mut parser, b"\x1b[6;18;9t");
    assert!(residue.is_empty());
    match &replies[0] {
        HostReply::PixelDimensions(pd) => {
            let ccs = pd.character_cell_size.expect("cell size");
            assert_eq!(ccs.height, 18);
            assert_eq!(ccs.width, 9);
        },
        other => panic!("unexpected reply: {:?}", other),
    }
}

#[test]
fn background_color_reply() {
    let mut parser = StdinAnsiParser::new();
    let (replies, residue) = feed_once(&mut parser, b"\x1b]11;rgb:0000/0000/0000\x1b\\");
    assert!(residue.is_empty());
    match &replies[0] {
        HostReply::BackgroundColor(s) => assert_eq!(s, "rgb:0000/0000/0000"),
        other => panic!("unexpected reply: {:?}", other),
    }
}

#[test]
fn foreground_color_reply() {
    let mut parser = StdinAnsiParser::new();
    let (replies, residue) = feed_once(&mut parser, b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\");
    assert!(residue.is_empty());
    match &replies[0] {
        HostReply::ForegroundColor(s) => assert_eq!(s, "rgb:ffff/ffff/ffff"),
        other => panic!("unexpected reply: {:?}", other),
    }
}

#[test]
fn color_register_reply() {
    let mut parser = StdinAnsiParser::new();
    let (replies, residue) = feed_once(&mut parser, b"\x1b]4;5;rgb:8080/8080/8080\x1b\\");
    assert!(residue.is_empty());
    match &replies[0] {
        HostReply::ColorRegisters(regs) => {
            assert_eq!(regs.len(), 1);
            assert_eq!(regs[0].0, 5);
            assert_eq!(regs[0].1, "rgb:8080/8080/8080");
        },
        other => panic!("unexpected reply: {:?}", other),
    }
}

#[test]
fn synchronized_output_supported_reply() {
    let mut parser = StdinAnsiParser::new();
    let (replies, residue) = feed_once(&mut parser, b"\x1b[?2026;1$y");
    assert!(residue.is_empty());
    match &replies[0] {
        HostReply::SynchronizedOutput(Some(_)) => {},
        other => panic!("unexpected reply: {:?}", other),
    }
}

#[test]
fn keyboard_residue_passes_through_unchanged() {
    // Arrow key escape sequence is NOT a whitelisted CSI report (final
    // byte 'A'), so it must survive as keyboard residue verbatim.
    let mut parser = StdinAnsiParser::new();
    let (replies, residue) = feed_once(&mut parser, b"\x1b[A");
    assert!(replies.is_empty());
    assert_eq!(residue, b"\x1b[A");
}

#[test]
fn mixed_keyboard_and_reply_extracts_both_cleanly() {
    // Arrow keys bracketing a pixel-dim reply — residue should be just
    // the arrow-key bytes, reply should be classified.
    let mut parser = StdinAnsiParser::new();
    let mut input = Vec::new();
    input.extend_from_slice(b"\x1b[A");
    input.extend_from_slice(b"\x1b[4;720;1280t");
    input.extend_from_slice(b"\x1b[B");
    let (replies, residue) = feed_once(&mut parser, &input);
    assert_eq!(replies.len(), 1);
    matches!(replies[0], HostReply::PixelDimensions(_));
    assert_eq!(residue, b"\x1b[A\x1b[B");
}

#[test]
fn unterminated_osc_within_single_chunk_is_buffered() {
    // An OSC that never terminates within a chunk is held in the
    // partial-OSC buffer for the next call to complete; nothing leaks
    // into residue. This is the cross-chunk-aware replacement for the
    // pre-fix behaviour where unterminated OSC bytes fell through to
    // residue and surfaced as spurious keypresses.
    let mut parser = StdinAnsiParser::new();
    let (_replies, residue) = feed_once(&mut parser, b"\x1b]10;partial");
    assert!(
        residue.is_empty(),
        "unterminated OSC must be buffered, not leaked to residue: {:?}",
        residue
    );
}

#[test]
fn forwarding_window_accumulates_and_barrier_closes() {
    let mut parser = StdinAnsiParser::new();
    parser.open_forward(42);
    // Feed an OSC 11 reply, then the Primary-DA barrier. Use color
    // bytes that do NOT contain `c` so the barrier-absence assertion
    // below can use a simple byte search.
    let mut chunk = Vec::new();
    chunk.extend_from_slice(b"\x1b]11;rgb:aaaa/bbbb/dddd\x1b\\");
    chunk.extend_from_slice(b"\x1b[?65;1c");
    let out = parser.feed(&chunk);
    // OSC 11 was classified (double-dispatch), and the barrier itself
    // advertises the host's sixel capability.
    assert_eq!(out.replies.len(), 2);
    matches!(out.replies[0], HostReply::BackgroundColor(_));
    matches!(out.replies[1], HostReply::SixelSupport(false));
    // Barrier closed the window, producing a completed forward.
    let (token, reply_bytes) = out
        .completed_forward
        .expect("barrier should close the window");
    assert_eq!(token, 42);
    // Reply bytes should contain the OSC 11 (re-serialized) but NOT the
    // barrier reply itself.
    assert!(
        reply_bytes.windows(5).any(|w| w == b"]11;r"),
        "OSC 11 should be in the forwarded buffer: {:?}",
        reply_bytes
    );
    assert!(
        !reply_bytes.contains(&b'c'),
        "Primary-DA barrier (final byte 'c') must not appear in forwarded reply"
    );
    // Slot released.
    assert!(parser.active_forward_token().is_none());
}

#[test]
fn unsolicited_osc_between_forwarded_query_and_barrier() {
    // Scenario: host emits a stray OSC 10 between the app's OSC 11 query
    // and the barrier. Both replies should end up in the forwarded
    // buffer, and the barrier closes the window.
    let mut parser = StdinAnsiParser::new();
    parser.open_forward(7);
    let mut chunk = Vec::new();
    chunk.extend_from_slice(b"\x1b]11;rgb:1111/1111/1111\x1b\\");
    chunk.extend_from_slice(b"\x1b]10;rgb:2222/2222/2222\x1b\\");
    chunk.extend_from_slice(b"\x1b[c");
    let out = parser.feed(&chunk);
    assert_eq!(out.replies.len(), 2);
    let (token, reply_bytes) = out.completed_forward.unwrap();
    assert_eq!(token, 7);
    // Both OSCs present.
    assert!(reply_bytes.windows(4).any(|w| w == b"]11;"));
    assert!(reply_bytes.windows(4).any(|w| w == b"]10;"));
}

#[test]
fn double_dispatch_without_active_forward_still_emits_reply() {
    let mut parser = StdinAnsiParser::new();
    // No open_forward — reply should still be classified.
    let out = parser.feed(b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\");
    assert_eq!(out.replies.len(), 1);
    matches!(out.replies[0], HostReply::BackgroundColor(_));
    assert!(out.completed_forward.is_none());
}

#[test]
fn timeout_flushes_accumulated_bytes() {
    let mut parser = StdinAnsiParser::new();
    parser.open_forward(99);
    let out = parser.feed(b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\");
    assert!(out.completed_forward.is_none(), "no barrier yet");
    assert!(parser.active_forward_token() == Some(99));
    // Simulate timeout firing on the watcher.
    let flushed = parser.close_forward_on_timeout(99);
    let (token, bytes) = flushed.expect("timeout flush should produce a payload");
    assert_eq!(token, 99);
    assert!(bytes.windows(4).any(|w| w == b"]11;"));
    assert!(parser.active_forward_token().is_none());
}

#[test]
fn stale_token_timeout_does_nothing() {
    let mut parser = StdinAnsiParser::new();
    parser.open_forward(1);
    // Ask to timeout a different token — nothing happens.
    assert!(parser.close_forward_on_timeout(999).is_none());
    assert_eq!(parser.active_forward_token(), Some(1));
}

#[test]
fn fragmented_osc_does_not_leak_into_residue() {
    let full = b"\x1b]11;rgb:0000/0000/0000\x1b\\";
    for split in 1..full.len() {
        let mut p = StdinAnsiParser::new();
        let r1 = p.feed(&full[..split]);
        let r2 = p.feed(&full[split..]);
        assert!(
            r1.residue.is_empty(),
            "split at {}: chunk 1 residue should be empty, got {:?}",
            split,
            r1.residue
        );
        assert!(
            r2.residue.is_empty(),
            "split at {}: chunk 2 residue should be empty, got {:?}",
            split,
            r2.residue
        );
        assert_eq!(
            r1.replies.len() + r2.replies.len(),
            1,
            "split at {}: exactly one reply across both chunks",
            split
        );
    }
}

#[test]
fn fragmented_csi_report_does_not_leak() {
    // Pixel-dimensions reply (final byte 't').
    let full = b"\x1b[4;800;1200t";
    for split in 1..full.len() {
        let mut p = StdinAnsiParser::new();
        let r1 = p.feed(&full[..split]);
        let r2 = p.feed(&full[split..]);
        assert!(
            r1.residue.is_empty(),
            "split at {}: c1 residue {:?}",
            split,
            r1.residue
        );
        assert!(
            r2.residue.is_empty(),
            "split at {}: c2 residue {:?}",
            split,
            r2.residue
        );
        assert_eq!(r1.replies.len() + r2.replies.len(), 1);
    }
}

#[test]
fn fragmented_osc_byte_by_byte() {
    // Every byte in its own chunk — the worst case.
    let full = b"\x1b]11;rgb:abcd/ef01/2345\x1b\\";
    let mut p = StdinAnsiParser::new();
    let mut total_replies = 0;
    for &b in full {
        let out = p.feed(&[b]);
        assert!(out.residue.is_empty(), "byte 0x{:02x} leaked to residue", b);
        total_replies += out.replies.len();
    }
    assert_eq!(total_replies, 1);
}

#[test]
fn lone_trailing_esc_is_buffered_then_finalized_as_residue() {
    // A bare ESC byte at the tail of a chunk could be the start of an
    // OSC or CSI host-reply that's been fragmented at the ESC
    // boundary, so `feed` parks it under partial state instead of
    // leaking it as a keyboard residue. But the byte must not stay
    // parked forever — `finalize()` is the idle drain that releases
    // it back to the keyboard parser when no follow-up arrives.
    let mut p = StdinAnsiParser::new();
    let out = p.feed(b"\x1b");
    assert!(
        out.residue.is_empty(),
        "lone ESC must not leak immediately: {:?}",
        out.residue
    );
    assert!(
        out.has_partial_state,
        "lone ESC must mark has_partial_state so the caller schedules a finalize tick"
    );
    assert_eq!(p.pending_partial(), PendingPartial::LoneEsc);
    let drained = p.finalize_fast_partial();
    assert_eq!(
        drained,
        vec![0x1b],
        "finalize must release the parked ESC as keyboard residue"
    );
    // Subsequent finalize is a no-op once the parker is empty.
    assert!(p.finalize_fast_partial().is_empty());
    assert_eq!(p.pending_partial(), PendingPartial::None);
}

#[test]
fn fragmented_osc_does_not_finalize_partial() {
    // Inverse case: a real fragmented host-reply (ESC then `]...`
    // arrived in two chunks with no idle between them) must NOT be
    // released by finalize. The key signal is timing — if the second
    // chunk arrives quickly enough the idle path is never taken, and
    // the OSC completes normally. We exercise that order here.
    let full = b"\x1b]11;rgb:0000/0000/0000\x1b\\";
    let mut p = StdinAnsiParser::new();
    let r1 = p.feed(&full[..1]);
    assert!(r1.residue.is_empty());
    assert!(r1.has_partial_state);
    let r2 = p.feed(&full[1..]);
    assert!(r2.residue.is_empty(), "tail must complete the OSC");
    assert_eq!(r1.replies.len() + r2.replies.len(), 1);
    assert!(
        p.finalize_force().is_empty(),
        "no partial left after completion"
    );
}


#[test]
fn osc_99_routes_into_desktop_notifications() {
    // OSC 99 is the desktop-notification response. It must surface in
    // ParseOutput.desktop_notifications (where the stdin handler routes
    // it as InputInstruction::DesktopNotificationResponse), NOT in
    // residue (which would never reach the keyboard parser anyway,
    // since the scrubber strips OSC bytes).
    let mut p = StdinAnsiParser::new();
    let out = p.feed(b"\x1b]99;notification body\x1b\\");
    assert!(out.residue.is_empty(), "OSC 99 must not leak to residue");
    assert!(out.replies.is_empty(), "OSC 99 is not a HostReply variant");
    assert_eq!(out.desktop_notifications.len(), 1);
    assert_eq!(out.desktop_notifications[0], b"notification body".to_vec());
}

#[test]
fn osc_99_does_not_leak_into_an_open_forwarding_window() {
    let mut parser = StdinAnsiParser::new();
    parser.open_forward(7);
    let mut chunk = Vec::new();
    chunk.extend_from_slice(b"\x1b]99;i=p1.myid;\x1b\\");
    chunk.extend_from_slice(b"\x1b]11;rgb:1111/1111/1111\x1b\\");
    chunk.extend_from_slice(b"\x1b[c");
    let out = parser.feed(&chunk);
    assert_eq!(out.desktop_notifications.len(), 1);
    let (token, reply_bytes) = out.completed_forward.unwrap();
    assert_eq!(token, 7);
    assert!(
        reply_bytes.windows(4).any(|w| w == b"]11;"),
        "the reply the pane asked for is forwarded: {:?}",
        reply_bytes
    );
    assert!(
        !reply_bytes.windows(4).any(|w| w == b"]99;"),
        "the notification response is not: {:?}",
        reply_bytes
    );
}

#[test]
fn fragmented_osc_99_emits_one_notification() {
    // Cross-chunk regression: OSC 99 split across two feed() calls
    // must still emit exactly one notification, with no leak into
    // residue.
    let full = b"\x1b]99;hello world\x1b\\";
    for split in 1..full.len() {
        let mut p = StdinAnsiParser::new();
        let r1 = p.feed(&full[..split]);
        let r2 = p.feed(&full[split..]);
        assert!(r1.residue.is_empty(), "split at {}: c1 residue", split);
        assert!(r2.residue.is_empty(), "split at {}: c2 residue", split);
        let total = r1.desktop_notifications.len() + r2.desktop_notifications.len();
        assert_eq!(total, 1, "split at {}: exactly one notification", split);
    }
}

#[test]
fn malformed_osc_still_does_not_eat_following_keyboard_bytes() {
    // Pre-existing invariant. An unterminated OSC followed (in a later
    // chunk) by plain keyboard input — the keyboard input must reach
    // residue intact once the OSC closes via proper flush.
    let mut p = StdinAnsiParser::new();
    let _ = p.feed(b"\x1b]10;partial");
    let _ = p.feed(b"\x1b\\");
    let out = p.feed(b"hello");
    assert_eq!(out.residue, b"hello");
}

#[test]
fn cross_chunk_osc_assembles_across_feeds() {
    // An OSC 11 reply split across two `feed()` calls. The inner
    // `InputParser` buffers OSC state across calls, so the second
    // chunk's terminator completes the sequence and the parser
    // emits a single classified reply. The first chunk produces no
    // reply; what lands in residue on the first chunk is byte-level
    // scrubber behaviour and intentionally not pinned here (see the
    // follow-up chunked_residue tests for those shapes).
    let mut parser = StdinAnsiParser::new();
    let first = parser.feed(b"\x1b]11;rgb:ffff/");
    assert!(
        first.replies.is_empty(),
        "first chunk must not classify a reply yet: {:?}",
        first.replies
    );

    let second = parser.feed(b"ffff/ffff\x1b\\");
    assert_eq!(
        second.replies.len(),
        1,
        "second chunk completes the OSC; parser emits one reply: {:?}",
        second.replies
    );
    match &second.replies[0] {
        HostReply::BackgroundColor(s) => assert_eq!(s, "rgb:ffff/ffff/ffff"),
        other => panic!("expected BackgroundColor, got {:?}", other),
    }
}

#[test]
fn double_dispatch_matrix_with_forward_active() {
    // For each whitelisted reply variant, opening a forward window and
    // feeding the reply must (a) classify the variant into
    // `ParseOutput.replies` and (b) accumulate the raw bytes into the
    // forward's reply buffer — both paths always fire, so cached state
    // and the forwarded-to pane stay in sync. OSC 11 is already
    // covered by `forwarding_window_accumulates_and_barrier_closes`;
    // this test sweeps OSC 10, OSC 4, CSI 14t / 16t replies, and
    // DECRPM 2026.
    let cases: Vec<(&[u8], fn(&HostReply) -> bool, &str)> = vec![
        (
            b"\x1b]10;rgb:1111/2222/3333\x1b\\",
            |r| matches!(r, HostReply::ForegroundColor(_)),
            "OSC 10",
        ),
        (
            b"\x1b]4;9;rgb:4444/5555/6666\x1b\\",
            |r| matches!(r, HostReply::ColorRegisters(_)),
            "OSC 4",
        ),
        (
            b"\x1b[4;720;1280t",
            |r| matches!(r, HostReply::PixelDimensions(_)),
            "CSI 14t reply",
        ),
        (
            b"\x1b[6;18;9t",
            |r| matches!(r, HostReply::PixelDimensions(_)),
            "CSI 16t reply",
        ),
        (
            b"\x1b[?2026;1$y",
            |r| matches!(r, HostReply::SynchronizedOutput(_)),
            "DECRPM 2026",
        ),
    ];
    for (bytes, is_expected_variant, label) in cases {
        let mut parser = StdinAnsiParser::new();
        parser.open_forward(11);
        let out = parser.feed(bytes);
        assert_eq!(
            out.replies.len(),
            1,
            "{}: should classify exactly one reply (got {:?})",
            label,
            out.replies
        );
        assert!(
            is_expected_variant(&out.replies[0]),
            "{}: wrong variant {:?}",
            label,
            out.replies[0]
        );
        assert!(
            out.completed_forward.is_none(),
            "{}: no barrier yet, slot must stay open",
            label
        );
        // Close the window to inspect the forward buffer.
        let (token, raw) = parser
            .close_forward_on_timeout(11)
            .expect("forward slot should still be open");
        assert_eq!(token, 11, "{}: token preserved", label);
        assert!(
            !raw.is_empty(),
            "{}: reply bytes must have been accumulated into the forward buffer",
            label
        );
    }
}

#[test]
fn primary_da_barrier_accepts_extended_forms() {
    // The barrier is "any CSI reply with final byte `c`". Real hosts
    // emit parameters (`\x1b[?62;1;6c`) or the secondary-DA-esque
    // prefix (`\x1b[>0;276;0c`). Both must close the forward window
    // the same way the bare `\x1b[c` does.
    for barrier in [
        b"\x1b[c".as_ref(),
        b"\x1b[?62;1;6c".as_ref(),
        b"\x1b[>0;276;0c".as_ref(),
    ] {
        let mut parser = StdinAnsiParser::new();
        parser.open_forward(5);
        let mut chunk = Vec::new();
        chunk.extend_from_slice(b"\x1b]11;rgb:aaaa/bbbb/cccc\x1b\\");
        chunk.extend_from_slice(barrier);
        let out = parser.feed(&chunk);
        let (token, reply_bytes) = out
            .completed_forward
            .expect("every Primary-DA reply form must close the forward window");
        assert_eq!(token, 5);
        assert!(
            reply_bytes.windows(4).any(|w| w == b"]11;"),
            "OSC 11 should be present in the forwarded buffer for barrier {:?}",
            std::str::from_utf8(barrier).unwrap_or("<non-utf8>")
        );
        assert!(parser.active_forward_token().is_none());
    }
}

// =====================================================================
// `schedule_forward_timeout` — async timer, driven by paused tokio clock
// =====================================================================

/// Build a paused current-thread runtime with `enable_time()`. With
/// `start_paused(true)` the clock only advances when we explicitly
/// call `tokio::time::advance`, so tests don't wall-clock sleep.
fn paused_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .expect("paused runtime build must succeed")
}

#[test]
fn timer_fires_after_deadline_and_closes_slot() {
    let rt = paused_runtime();
    let parser = Arc::new(Mutex::new(StdinAnsiParser::new()));
    parser.lock().unwrap().open_forward(7);

    let captured: Arc<Mutex<Option<(u32, Vec<u8>)>>> = Arc::new(Mutex::new(None));
    let captured_clone = captured.clone();
    schedule_forward_timeout(
        rt.handle(),
        parser.clone(),
        7,
        Duration::from_millis(500),
        move |token, bytes| {
            *captured_clone.lock().unwrap() = Some((token, bytes));
        },
    );

    // Drive the runtime enough to run the just-spawned task up to its
    // sleep point, then jump past the deadline.
    rt.block_on(async {
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(600)).await;
        // One more yield so the woken task can complete synchronously.
        tokio::task::yield_now().await;
    });

    let (token, bytes) = captured
        .lock()
        .unwrap()
        .take()
        .expect("timer must have invoked on_timeout");
    assert_eq!(token, 7);
    assert!(
        bytes.is_empty(),
        "nothing was fed to the parser → buffer is empty"
    );
    assert!(
        parser.lock().unwrap().active_forward_token().is_none(),
        "slot must have been cleared by the timer"
    );
}

#[test]
fn timer_is_noop_when_barrier_already_closed_the_slot() {
    let rt = paused_runtime();
    let parser = Arc::new(Mutex::new(StdinAnsiParser::new()));
    parser.lock().unwrap().open_forward(11);

    let fired: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let fired_clone = fired.clone();
    schedule_forward_timeout(
        rt.handle(),
        parser.clone(),
        11,
        Duration::from_millis(500),
        move |_, _| {
            *fired_clone.lock().unwrap() = true;
        },
    );

    // Before the deadline, simulate the barrier arriving: feed a reply
    // and the Primary-DA barrier; the parser's `completed_forward`
    // path clears the slot.
    {
        let mut p = parser.lock().unwrap();
        let mut chunk = Vec::new();
        chunk.extend_from_slice(b"\x1b]11;rgb:0/0/0\x1b\\");
        chunk.extend_from_slice(b"\x1b[c");
        let out = p.feed(&chunk);
        assert!(out.completed_forward.is_some(), "barrier should close slot");
    }

    // Now let the timer wake up — token-guard idempotency should make
    // it a no-op.
    rt.block_on(async {
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(600)).await;
        tokio::task::yield_now().await;
    });

    assert!(
        !*fired.lock().unwrap(),
        "timer must not invoke on_timeout once the barrier has closed the slot"
    );
}

#[test]
fn timer_is_noop_when_slot_holds_a_different_token() {
    // This models re-entry *after* the server has moved on: the old
    // timer's token no longer matches the active slot, so
    // `close_forward_on_timeout(old_token)` returns None and the
    // callback doesn't fire.
    let rt = paused_runtime();
    let parser = Arc::new(Mutex::new(StdinAnsiParser::new()));
    parser.lock().unwrap().open_forward(1);

    let fired: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let fired_clone = fired.clone();
    schedule_forward_timeout(
        rt.handle(),
        parser.clone(),
        1,
        Duration::from_millis(500),
        move |_, _| {
            *fired_clone.lock().unwrap() = true;
        },
    );

    // Close slot for token 1 via the barrier path, then open a fresh
    // slot for a different token. The earlier spawned timer is still
    // sleeping and holds a snapshot of token=1.
    {
        let mut p = parser.lock().unwrap();
        let mut chunk = Vec::new();
        chunk.extend_from_slice(b"\x1b[c");
        let _ = p.feed(&chunk); // close via barrier
        p.open_forward(2);
    }

    rt.block_on(async {
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(600)).await;
        tokio::task::yield_now().await;
    });

    assert!(
        !*fired.lock().unwrap(),
        "stale timer must not close the new slot"
    );
    assert_eq!(
        parser.lock().unwrap().active_forward_token(),
        Some(2),
        "new slot untouched"
    );
}

#[test]
fn timer_preserves_accumulated_reply_bytes_on_timeout() {
    // If the host went silent after emitting partial replies, the
    // timer still has to flush whatever landed in the buffer so the
    // pane sees *something* — empty is fine, partial is better.
    let rt = paused_runtime();
    let parser = Arc::new(Mutex::new(StdinAnsiParser::new()));
    parser.lock().unwrap().open_forward(22);

    // Simulate a single OSC 11 reply arriving before the host goes
    // silent.
    {
        let mut p = parser.lock().unwrap();
        let _ = p.feed(b"\x1b]11;rgb:1234/5678/9abc\x1b\\");
    }

    let captured: Arc<Mutex<Option<(u32, Vec<u8>)>>> = Arc::new(Mutex::new(None));
    let captured_clone = captured.clone();
    schedule_forward_timeout(
        rt.handle(),
        parser.clone(),
        22,
        Duration::from_millis(500),
        move |t, b| {
            *captured_clone.lock().unwrap() = Some((t, b));
        },
    );

    rt.block_on(async {
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(600)).await;
        tokio::task::yield_now().await;
    });

    let (token, bytes) = captured.lock().unwrap().take().expect("timer must fire");
    assert_eq!(token, 22);
    assert!(
        bytes.windows(4).any(|w| w == b"]11;"),
        "buffered OSC 11 bytes must appear in the flushed payload: {:?}",
        bytes
    );
}

#[test]
fn host_theme_dsr_997_dark_in_one_chunk() {
    let mut parser = StdinAnsiParser::new();
    let (replies, residue) = feed_once(&mut parser, b"\x1b[?997;1n");
    assert!(residue.is_empty(), "DSR 997 reply must be fully consumed");
    assert_eq!(replies.len(), 1);
    match &replies[0] {
        HostReply::HostTerminalThemeChanged(mode) => {
            assert_eq!(*mode, zellij_utils::data::HostTerminalThemeMode::Dark);
        },
        other => panic!("expected HostTerminalThemeChanged, got {:?}", other),
    }
}

#[test]
fn host_theme_dsr_997_light_in_one_chunk() {
    let mut parser = StdinAnsiParser::new();
    let (replies, residue) = feed_once(&mut parser, b"\x1b[?997;2n");
    assert!(residue.is_empty());
    match &replies[0] {
        HostReply::HostTerminalThemeChanged(mode) => {
            assert_eq!(*mode, zellij_utils::data::HostTerminalThemeMode::Light);
        },
        other => panic!("expected HostTerminalThemeChanged, got {:?}", other),
    }
}

#[test]
fn host_theme_dsr_997_across_chunk_boundaries() {
    // Same coverage philosophy as fragmented_csi_report_does_not_leak:
    // split the DSR at every internal byte position and assert the
    // parser still emits exactly one HostReply with no leaked residue.
    let full = b"\x1b[?997;1n";
    for split in 1..full.len() {
        let mut p = StdinAnsiParser::new();
        let r1 = p.feed(&full[..split]);
        let r2 = p.feed(&full[split..]);
        assert!(
            r1.residue.is_empty(),
            "split at {}: chunk 1 leaked residue {:?}",
            split,
            r1.residue
        );
        assert!(
            r2.residue.is_empty(),
            "split at {}: chunk 2 leaked residue {:?}",
            split,
            r2.residue
        );
        let total = r1.replies.len() + r2.replies.len();
        assert_eq!(total, 1, "split at {}: got {} replies", split, total);
        let reply = r1.replies.into_iter().chain(r2.replies).next().unwrap();
        match reply {
            HostReply::HostTerminalThemeChanged(mode) => {
                assert_eq!(mode, zellij_utils::data::HostTerminalThemeMode::Dark);
            },
            other => panic!("expected HostTerminalThemeChanged, got {:?}", other),
        }
    }
}

#[test]
fn host_theme_dsr_997_unknown_param_dropped() {
    let mut parser = StdinAnsiParser::new();
    // CSI ?997;3n is not a defined Dark/Light value; the parser
    // recognises the CSI shape (final byte n) but
    // `from_csi_report` rejects unknown payloads, so no HostReply is
    // produced and no residue leaks (the bytes were consumed by termwiz).
    let out = parser.feed(b"\x1b[?997;3n");
    assert!(
        out.replies.is_empty(),
        "unknown ?997;N value must not classify"
    );
}

// =====================================================================
// Regression test: a Kitty keyboard-protocol event arriving on the
// client's stdin must not wedge the parser. Before the fix, such an
// event (`\x1b[<keycode>;<mods>u`) sat at the head of termwiz's
// internal buffer because parse_csi_report rejects the unwhitelisted
// final byte 'u' AND the keymap returned `Found::NeedData` (it's a
// possible prefix of registered key sequences). Every host reply that
// arrived behind it was silently swallowed, stalling host-color
// forwards until session timeout. This was confirmed via WEDGE-CANDIDATE
// log entries from three independent users on Kitty, Ghostty, and
// Konsole — all triggered by the same `\x1b[<digits>;<digits>u` shape.
//
// Captured triggers from real logs:
//   `\x1b[57414;129u`  (Kitty user, repro-logs/third-commit/zellij(4).log)
//   `\x1b[27;129u`     (Ghostty user, ...zellij(5).log)
//   `\x1b[102;133u`    (Konsole user, ...zellij_0.45_new-2.log)
// =====================================================================

#[test]
fn kitty_kbd_event_does_not_wedge_subsequent_forward_reply() {
    let kbd_events: Vec<&[u8]> = vec![
        b"\x1b[57414;129u", // captured from Kitty user
        b"\x1b[27;129u",    // captured from Ghostty user
        b"\x1b[102;133u",   // captured from Konsole user
        b"\x1b[A",          // legacy CSI key (not Kitty kbd) — sanity
    ];

    for kbd in &kbd_events {
        let pretty = String::from_utf8_lossy(kbd).replace('\x1b', "ESC");
        let mut parser = StdinAnsiParser::new();

        // Pre-wedge: keypress arrives before the forward goes out.
        let _ = parser.feed(kbd);

        // Forward dispatched — slot opens for token=42.
        parser.open_forward(42);

        // Host's reply arrives in two chunks (matches captured wire:
        // OSC 11 reply 25B, then DA1 barrier 10B).
        let _ = parser.feed(b"\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\");
        let out = parser.feed(b"\x1b[?62;52;c");

        let (token, bytes) = out.completed_forward.unwrap_or_else(|| {
            panic!(
                "BARRIER never closed slot for kbd-event prelude {:?} — \
                 termwiz wedge regression",
                pretty
            )
        });
        assert_eq!(token, 42, "wrong token closed for prelude {:?}", pretty);
        assert!(
            !bytes.is_empty(),
            "OSC payload was lost behind the kbd event for prelude {:?}",
            pretty
        );
    }
}

use crate::keyboard_parser::{KittyKeyboardParser, KittyParseOutcome};
use zellij_utils::data::BareKey;
use zellij_utils::vendored::termwiz::input::{
    InputEvent, InputParser, KeyCode, KeyEvent, Modifiers,
};

fn assert_held_across_idle(parser: &mut StdinAnsiParser) {
    assert_eq!(
        parser.pending_partial(),
        PendingPartial::ReplyInProgress,
        "a payload-bearing partial must report ReplyInProgress while held"
    );
    assert!(
        parser.finalize_fast_partial().is_empty(),
        "the short idle finalize must NOT drain a reply-in-progress partial"
    );
    assert_eq!(
        parser.pending_partial(),
        PendingPartial::ReplyInProgress,
        "the partial must still be held after a lone-esc finalize"
    );
}

#[test]
fn fragmented_osc4_reply_is_retained_not_leaked() {
    let mut p = StdinAnsiParser::new();
    let r1 = p.feed(b"\x1b]4;1;rgb:1111");
    assert!(
        r1.residue.is_empty(),
        "incomplete OSC 4 must not leak: {:?}",
        r1.residue
    );
    assert!(r1.replies.is_empty());
    assert_held_across_idle(&mut p);

    let r2 = p.feed(b"/2222/3333\x1b\\");
    assert!(
        r2.residue.is_empty(),
        "completion must not leak: {:?}",
        r2.residue
    );
    assert_eq!(r2.replies.len(), 1, "the rejoined OSC 4 classifies once");
    match &r2.replies[0] {
        HostReply::ColorRegisters(regs) => {
            assert_eq!(regs[0].0, 1);
            assert_eq!(regs[0].1, "rgb:1111/2222/3333");
        },
        other => panic!("expected ColorRegisters, got {:?}", other),
    }
    assert_eq!(p.pending_partial(), PendingPartial::None);
}

#[test]
fn fragmented_osc11_reply_is_retained_across_idle() {
    let mut p = StdinAnsiParser::new();
    let r1 = p.feed(b"\x1b]11;rgb:0000/00");
    assert!(r1.residue.is_empty());
    assert_held_across_idle(&mut p);

    let r2 = p.feed(b"00/0000\x1b\\");
    assert!(r2.residue.is_empty());
    assert_eq!(r2.replies.len(), 1);
    matches!(r2.replies[0], HostReply::BackgroundColor(_));
}

#[test]
fn esc_companion_chord_is_not_held_as_reply() {
    let mut whole = StdinAnsiParser::new();
    let out = whole.feed(b"\x1ba");
    assert_eq!(out.residue, b"\x1ba");
    assert!(out.replies.is_empty());

    let mut split = StdinAnsiParser::new();
    let r1 = split.feed(b"\x1b");
    assert!(
        r1.residue.is_empty(),
        "lone ESC must park until disambiguated"
    );
    assert_eq!(split.pending_partial(), PendingPartial::LoneEsc);
    let r2 = split.feed(b"a");
    let mut combined = r1.residue.clone();
    combined.extend_from_slice(&r2.residue);
    assert_eq!(combined, b"\x1ba");
    assert!(r2.replies.is_empty());
}

#[test]
fn fragmented_ctrl_arrow_is_retained_then_passed_through() {
    let mut p = StdinAnsiParser::new();
    let r1 = p.feed(b"\x1b[1;5");
    assert!(r1.residue.is_empty(), "incomplete CSI must not leak");
    assert_held_across_idle(&mut p);

    let r2 = p.feed(b"C");
    assert_eq!(r2.residue, b"\x1b[1;5C", "the arrow reassembles intact");
    assert!(r2.replies.is_empty(), "a cursor key is never a host reply");
}

#[test]
fn fragmented_sgr_mouse_is_retained_then_passed_through() {
    let mut p = StdinAnsiParser::new();
    let r1 = p.feed(b"\x1b[<0;12;3");
    assert!(r1.residue.is_empty());
    assert_held_across_idle(&mut p);
    let r2 = p.feed(b"4M");
    assert_eq!(r2.residue, b"\x1b[<0;12;34M");
    assert!(r2.replies.is_empty());
}

#[test]
fn focus_event_is_residue_not_reply() {
    let mut whole = StdinAnsiParser::new();
    let out = whole.feed(b"\x1b[I");
    assert_eq!(out.residue, b"\x1b[I");
    assert!(out.replies.is_empty());

    let mut split = StdinAnsiParser::new();
    let r1 = split.feed(b"\x1b[");
    assert!(r1.residue.is_empty());
    let r2 = split.feed(b"I");
    let mut combined = r1.residue.clone();
    combined.extend_from_slice(&r2.residue);
    assert_eq!(combined, b"\x1b[I");
    assert!(r2.replies.is_empty());
}

#[test]
fn kitty_keyboard_variants_whole_and_fragmented() {
    let variants: Vec<&[u8]> = vec![
        b"\x1b[97u",
        b"\x1b[97;5u",
        b"\x1b[97;1:3u",
        b"\x1b[97:65;5u",
        b"\x1b[97;5;65u",
        b"\x1b[1;5A",
        b"\x1b[1;2H",
        b"\x1b[1;2P",
        b"\x1b[1;2S",
        b"\x1b[2;5~",
        b"\x1b[3;5~",
        b"\x1b[15;2~",
        b"\x1b[27u",
        b"\x1b[57362u",
        b"\x1b[Z",
    ];
    for seq in variants {
        let pretty = String::from_utf8_lossy(seq).replace('\x1b', "ESC");

        let mut whole = StdinAnsiParser::new();
        let out = whole.feed(seq);
        assert_eq!(
            out.residue, seq,
            "{}: whole residue must equal the sequence",
            pretty
        );
        assert!(out.replies.is_empty(), "{}: a key is never a reply", pretty);

        for split in 2..seq.len() {
            let mut p = StdinAnsiParser::new();
            let r1 = p.feed(&seq[..split]);
            assert!(
                r1.residue.is_empty(),
                "{} split at {}: prefix must be held, not leaked: {:?}",
                pretty,
                split,
                r1.residue
            );
            let expected_class = if split == 2 {
                PendingPartial::BareIntroducer
            } else {
                PendingPartial::ReplyInProgress
            };
            assert_eq!(
                p.pending_partial(),
                expected_class,
                "{} split at {}: unexpected partial classification",
                pretty,
                split
            );
            if split > 2 {
                assert!(
                    p.finalize_fast_partial().is_empty(),
                    "{} split at {}: idle must not drain the held key",
                    pretty,
                    split
                );
            }
            let r2 = p.feed(&seq[split..]);
            let mut combined = r1.residue.clone();
            combined.extend_from_slice(&r2.residue);
            assert_eq!(
                combined, seq,
                "{} split at {}: reassembled residue must equal the sequence",
                pretty, split
            );
            assert!(
                r1.replies.is_empty() && r2.replies.is_empty(),
                "{} split at {}: never classified as a reply",
                pretty,
                split
            );
        }
    }
}

fn keys_from_flushed_bytes(bytes: &[u8]) -> Vec<InputEvent> {
    let mut parser = InputParser::new();
    parser.parse_as_vec(bytes, false)
}

fn alt_char(c: char) -> InputEvent {
    InputEvent::Key(KeyEvent {
        key: KeyCode::Char(c),
        modifiers: Modifiers::ALT,
    })
}

#[test]
fn a_bare_osc_introducer_is_fast_flushed_as_alt_close_bracket() {
    let mut p = StdinAnsiParser::new();
    let out = p.feed(b"\x1b]");
    assert!(
        out.residue.is_empty(),
        "the introducer parks until disambiguated: {:?}",
        out.residue
    );
    assert_eq!(
        p.pending_partial(),
        PendingPartial::BareIntroducer,
        "a body-less ESC ] belongs to the fast-flush class"
    );
    assert!(
        !p.pending_partial().uses_reply_flush_guard(),
        "a bare introducer must not wait behind the long reply guard"
    );
    let drained = p.finalize_fast_partial();
    assert_eq!(
        drained,
        b"\x1b]".to_vec(),
        "the fast finalize must release the introducer"
    );
    assert_eq!(
        p.pending_partial(),
        PendingPartial::None,
        "the parser holds nothing after the fast flush"
    );
    assert_eq!(
        keys_from_flushed_bytes(&drained),
        vec![alt_char(']')],
        "the flushed bytes must decode as Alt+], not garbage"
    );
}

#[test]
fn a_bare_csi_introducer_is_fast_flushed_as_alt_open_bracket() {
    let mut p = StdinAnsiParser::new();
    let out = p.feed(b"\x1b[");
    assert!(
        out.residue.is_empty(),
        "the introducer parks until disambiguated: {:?}",
        out.residue
    );
    assert_eq!(
        p.pending_partial(),
        PendingPartial::BareIntroducer,
        "a body-less ESC [ belongs to the fast-flush class"
    );
    assert!(!p.pending_partial().uses_reply_flush_guard());
    let drained = p.finalize_fast_partial();
    assert_eq!(drained, b"\x1b[".to_vec());
    assert_eq!(p.pending_partial(), PendingPartial::None);
    assert_eq!(
        keys_from_flushed_bytes(&drained),
        vec![alt_char('[')],
        "the flushed bytes must decode as Alt+[, not garbage"
    );
}

#[test]
fn an_introducer_with_a_body_byte_keeps_the_long_reply_guard() {
    for prefix in [b"\x1b]4".as_ref(), b"\x1b[?".as_ref()] {
        let pretty = String::from_utf8_lossy(prefix).replace('\x1b', "ESC");
        let mut p = StdinAnsiParser::new();
        let out = p.feed(prefix);
        assert!(out.residue.is_empty(), "{}: must not leak", pretty);
        assert_eq!(
            p.pending_partial(),
            PendingPartial::ReplyInProgress,
            "{}: one body byte crosses the tier boundary",
            pretty
        );
        assert!(p.pending_partial().uses_reply_flush_guard());
        assert!(
            p.finalize_fast_partial().is_empty(),
            "{}: the fast flush must not touch a payload-bearing partial",
            pretty
        );
        assert_eq!(p.pending_partial(), PendingPartial::ReplyInProgress);
    }
}

#[test]
fn the_flush_path_is_selected_by_classification_not_by_wall_clock() {
    let cases: Vec<(&[u8], PendingPartial, bool)> = vec![
        (b"\x1b", PendingPartial::LoneEsc, false),
        (b"\x1b[", PendingPartial::BareIntroducer, false),
        (b"\x1b]", PendingPartial::BareIntroducer, false),
        (b"\x1b[1;5", PendingPartial::ReplyInProgress, true),
        (b"\x1b]4;1;rg", PendingPartial::ReplyInProgress, true),
    ];
    for (bytes, expected_class, guarded) in cases {
        let pretty = String::from_utf8_lossy(bytes).replace('\x1b', "ESC");
        let mut p = StdinAnsiParser::new();
        let _ = p.feed(bytes);
        assert_eq!(
            p.pending_partial(),
            expected_class,
            "{}: unexpected classification",
            pretty
        );
        assert_eq!(
            p.pending_partial().uses_reply_flush_guard(),
            guarded,
            "{}: the guard tier is decided by the classification alone",
            pretty
        );
        assert_eq!(
            p.finalize_fast_partial().is_empty(),
            guarded,
            "{}: the fast finalize drains exactly the unguarded classes",
            pretty
        );
    }
}

#[test]
fn an_osc_reply_fragmented_at_exactly_the_introducer_is_still_stripped() {
    let mut p = StdinAnsiParser::new();
    let r1 = p.feed(b"\x1b]");
    assert!(r1.residue.is_empty(), "the introducer must not leak");
    assert_eq!(p.pending_partial(), PendingPartial::BareIntroducer);

    let r2 = p.feed(b"4;1;rgb:1111/2222/3333\x1b\\");
    assert!(
        r2.residue.is_empty(),
        "a reply rejoined before any idle finalize must never leak as keys: {:?}",
        r2.residue
    );
    assert_eq!(r2.replies.len(), 1, "the rejoined OSC 4 classifies once");
    match &r2.replies[0] {
        HostReply::ColorRegisters(regs) => {
            assert_eq!(regs[0].0, 1);
            assert_eq!(regs[0].1, "rgb:1111/2222/3333");
        },
        other => panic!("expected ColorRegisters, got {:?}", other),
    }
    assert_eq!(p.pending_partial(), PendingPartial::None);
}

#[test]
fn a_csi_report_fragmented_at_exactly_the_introducer_is_still_stripped() {
    let mut p = StdinAnsiParser::new();
    let r1 = p.feed(b"\x1b[");
    assert!(r1.residue.is_empty());
    assert_eq!(p.pending_partial(), PendingPartial::BareIntroducer);

    let r2 = p.feed(b"?2026;1$y");
    assert!(
        r2.residue.is_empty(),
        "a DECRPM rejoined before any idle finalize must not leak: {:?}",
        r2.residue
    );
    assert_eq!(r2.replies.len(), 1);
    match &r2.replies[0] {
        HostReply::SynchronizedOutput(Some(_)) => {},
        other => panic!("unexpected reply: {:?}", other),
    }
    assert_eq!(p.pending_partial(), PendingPartial::None);
}

#[test]
fn kitty_ctrl_a_trace_yields_one_event_not_a_literal_u() {
    let mut p = StdinAnsiParser::new();
    let r1 = p.feed(b"\x1b[97;5");
    assert!(r1.residue.is_empty());
    assert_held_across_idle(&mut p);
    let r2 = p.feed(b"u");
    assert_eq!(r2.residue, b"\x1b[97;5u");

    let mut kitty = KittyKeyboardParser::new();
    match kitty.feed(&r2.residue) {
        KittyParseOutcome::Complete(key) => {
            assert!(
                key.is_key_with_ctrl_modifier(BareKey::Char('a')),
                "expected Ctrl-a, got {:?}",
                key
            );
        },
        other => panic!("expected a complete Ctrl-a, got {:?}", other),
    }
}

#[test]
fn bracketed_paste_preserves_embedded_escape_content() {
    let mut p = StdinAnsiParser::new();
    let mut chunk = Vec::new();
    chunk.extend_from_slice(b"\x1b[200~");
    chunk.extend_from_slice(b"echo \x1b]0;hi\x1b\\ done");
    chunk.extend_from_slice(b"\x1b[201~");
    let out = p.feed(&chunk);
    assert!(
        out.replies.is_empty(),
        "paste content must not classify as a reply: {:?}",
        out.replies
    );
    assert!(
        out.desktop_notifications.is_empty(),
        "an OSC inside a paste is literal, not a notification"
    );
    assert_eq!(
        out.residue, chunk,
        "the entire paste body, including the embedded OSC, passes through verbatim"
    );
    assert!(
        out.residue.windows(6).any(|w| w == b"\x1b]0;hi"),
        "the embedded OSC bytes must survive in residue"
    );
}

#[test]
fn paste_embedded_osc_is_not_eaten_when_fragmented() {
    let full: Vec<u8> = b"\x1b[200~A\x1b]11;rgb:0/0/0\x1b\\B\x1b[201~".to_vec();
    for split in 1..full.len() {
        let mut p = StdinAnsiParser::new();
        let r1 = p.feed(&full[..split]);
        let r2 = p.feed(&full[split..]);
        let mut combined = r1.residue.clone();
        combined.extend_from_slice(&r2.residue);
        assert_eq!(
            combined, full,
            "split at {}: paste body (with embedded OSC) must pass through whole",
            split
        );
        assert!(
            r1.replies.is_empty() && r2.replies.is_empty(),
            "split at {}: embedded OSC must not classify as a reply",
            split
        );
    }
}

#[test]
fn paste_markers_fragmented_still_toggle_paste_mode() {
    for (head, tail) in [(b"\x1b[20".as_ref(), b"0~".as_ref())] {
        let mut p = StdinAnsiParser::new();
        let r1 = p.feed(head);
        assert!(r1.residue.is_empty(), "fragmented start marker is held");
        let r2 = p.feed(tail);
        let mut start = r1.residue.clone();
        start.extend_from_slice(&r2.residue);
        assert_eq!(
            start, b"\x1b[200~",
            "start marker passes through to residue"
        );

        let body = p.feed(b"\x1b]0;x\x1b\\");
        assert_eq!(body.residue, b"\x1b]0;x\x1b\\");
        assert!(body.replies.is_empty());
    }
    for (head, tail) in [(b"\x1b[20".as_ref(), b"1~".as_ref())] {
        let mut p = StdinAnsiParser::new();
        let _ = p.feed(b"\x1b[200~body");
        let r1 = p.feed(head);
        let r2 = p.feed(tail);
        let mut end = r1.residue.clone();
        end.extend_from_slice(&r2.residue);
        assert_eq!(end, b"\x1b[201~", "end marker passes through to residue");

        let after = p.feed(b"\x1b]11;rgb:1/2/3\x1b\\");
        assert!(after.residue.is_empty());
        assert_eq!(after.replies.len(), 1);
    }
}

#[test]
fn last_resort_force_drain_recovers_a_never_terminated_partial() {
    let mut p = StdinAnsiParser::new();
    let r1 = p.feed(b"\x1b]4;1;rgb:incomplete");
    assert!(r1.residue.is_empty());
    assert_eq!(p.pending_partial(), PendingPartial::ReplyInProgress);

    let r2 = p.feed(b"and-more");
    assert!(r2.residue.is_empty());
    assert_eq!(p.pending_partial(), PendingPartial::ReplyInProgress);

    let drained = p.finalize_force();
    assert!(
        drained.windows(14).any(|w| w == b"rgb:incomplete"),
        "force-drain must recover the buffered partial: {:?}",
        drained
    );
    assert!(
        drained.windows(8).any(|w| w == b"and-more"),
        "force-drain must recover bytes appended before the guard: {:?}",
        drained
    );
    assert_eq!(
        p.pending_partial(),
        PendingPartial::None,
        "the buffer is empty after a force-drain"
    );
}

#[test]
fn kitty_probe_ok_reply_classifies_true() {
    let mut parser = StdinAnsiParser::new();
    parser.expect_kitty_probe_reply();
    let (replies, residue) = feed_once(&mut parser, b"\x1b_Gi=31;OK\x1b\\");
    assert!(residue.is_empty(), "probe reply must be fully consumed");
    assert_eq!(replies.len(), 1);
    match &replies[0] {
        HostReply::KittyGraphicsSupport(supported) => assert!(*supported),
        other => panic!("unexpected reply: {:?}", other),
    }
}

#[test]
fn kitty_probe_error_reply_classifies_false() {
    let mut parser = StdinAnsiParser::new();
    parser.expect_kitty_probe_reply();
    let (replies, residue) = feed_once(&mut parser, b"\x1b_Gi=31;ENOTSUPPORTED:query failed\x1b\\");
    assert!(residue.is_empty());
    assert_eq!(replies.len(), 1);
    match &replies[0] {
        HostReply::KittyGraphicsSupport(supported) => assert!(!*supported),
        other => panic!("unexpected reply: {:?}", other),
    }
}

#[test]
fn kitty_probe_absence_resolves_false_on_barrier() {
    let mut parser = StdinAnsiParser::new();
    parser.expect_kitty_probe_reply();
    let (replies, residue) = feed_once(&mut parser, b"\x1b[?62;4;52c");
    assert!(residue.is_empty());
    let kitty_replies: Vec<bool> = replies
        .iter()
        .filter_map(|r| match r {
            HostReply::KittyGraphicsSupport(supported) => Some(*supported),
            _ => None,
        })
        .collect();
    assert_eq!(kitty_replies, vec![false]);
}

#[test]
fn primary_da_with_sixel_attribute_classifies_sixel_support_true() {
    let mut parser = StdinAnsiParser::new();
    let (replies, residue) = feed_once(&mut parser, b"\x1b[?62;4;52c");
    assert!(residue.is_empty());
    let sixel_replies: Vec<bool> = replies
        .iter()
        .filter_map(|r| match r {
            HostReply::SixelSupport(supported) => Some(*supported),
            _ => None,
        })
        .collect();
    assert_eq!(sixel_replies, vec![true]);
}

#[test]
fn primary_da_without_sixel_attribute_classifies_sixel_support_false() {
    let mut parser = StdinAnsiParser::new();
    let (replies, residue) = feed_once(&mut parser, b"\x1b[?62;22c");
    assert!(residue.is_empty());
    let sixel_replies: Vec<bool> = replies
        .iter()
        .filter_map(|r| match r {
            HostReply::SixelSupport(supported) => Some(*supported),
            _ => None,
        })
        .collect();
    assert_eq!(sixel_replies, vec![false]);
}

#[test]
fn primary_da_sixel_attribute_is_not_matched_as_a_substring() {
    let mut parser = StdinAnsiParser::new();
    let (replies, residue) = feed_once(&mut parser, b"\x1b[?64;14;21;22c");
    assert!(residue.is_empty());
    let sixel_replies: Vec<bool> = replies
        .iter()
        .filter_map(|r| match r {
            HostReply::SixelSupport(supported) => Some(*supported),
            _ => None,
        })
        .collect();
    assert_eq!(sixel_replies, vec![false]);
}

#[test]
fn kitty_probe_reply_then_barrier_emits_exactly_one_reply() {
    let mut parser = StdinAnsiParser::new();
    parser.expect_kitty_probe_reply();
    let mut chunk = Vec::new();
    chunk.extend_from_slice(b"\x1b_Gi=31;OK\x1b\\");
    chunk.extend_from_slice(b"\x1b[?62;4;52c");
    let (replies, residue) = feed_once(&mut parser, &chunk);
    assert!(residue.is_empty());
    let kitty_replies: Vec<bool> = replies
        .iter()
        .filter_map(|r| match r {
            HostReply::KittyGraphicsSupport(supported) => Some(*supported),
            _ => None,
        })
        .collect();
    assert_eq!(kitty_replies, vec![true]);
}

#[test]
fn kitty_probe_reply_fragmented_across_feeds() {
    let full = b"\x1b_Gi=31;OK\x1b\\";
    for split in 1..full.len() {
        let mut parser = StdinAnsiParser::new();
        parser.expect_kitty_probe_reply();
        let r1 = parser.feed(&full[..split]);
        let r2 = parser.feed(&full[split..]);
        assert!(
            r1.residue.is_empty(),
            "split at {}: chunk 1 residue {:?}",
            split,
            r1.residue
        );
        assert!(
            r2.residue.is_empty(),
            "split at {}: chunk 2 residue {:?}",
            split,
            r2.residue
        );
        assert_eq!(
            r1.replies.len() + r2.replies.len(),
            1,
            "split at {}: exactly one reply across both chunks",
            split
        );
        let reply = r1.replies.into_iter().chain(r2.replies).next().unwrap();
        match reply {
            HostReply::KittyGraphicsSupport(supported) => assert!(supported),
            other => panic!("split at {}: unexpected reply {:?}", split, other),
        }
    }
}

#[test]
fn apc_outside_probe_window_is_not_classified() {
    let mut parser = StdinAnsiParser::new();
    let out = parser.feed(b"\x1b_Gi=31;OK\x1b\\");
    assert!(out.replies.is_empty());
}

#[test]
fn unsolicited_kitty_graphics_reply_never_becomes_keyboard_input() {
    let mut parser = StdinAnsiParser::new();
    parser.expect_kitty_probe_reply();
    let out = parser.feed(b"\x1b_Gi=31;OK\x1b\\");
    assert_eq!(out.replies.len(), 1);

    let out = parser.feed(b"\x1b_Gi=2000000000;OK\x1b\\");
    assert!(
        out.residue.is_empty(),
        "a host reply to our own image transmission must not be typed into the focused pane, got: {:?}",
        out.residue
    );
    assert!(out.replies.is_empty());

    let mut fragmented = StdinAnsiParser::new();
    fragmented.expect_kitty_probe_reply();
    let _ = fragmented.feed(b"\x1b_Gi=31;OK\x1b\\");
    let first = fragmented.feed(b"\x1b_Gi=2000000");
    let second = fragmented.feed(b"000;OK\x1b\\");
    assert!(first.residue.is_empty() && second.residue.is_empty());
}

#[test]
fn non_kitty_apc_sequences_are_left_alone() {
    let mut parser = StdinAnsiParser::new();
    parser.expect_kitty_probe_reply();
    let out = parser.feed(b"\x1b_Xsomething\x1b\\");
    assert!(
        !out.residue.is_empty(),
        "only kitty graphics APCs may be swallowed"
    );
}

#[test]
fn unsolicited_theme_notification_classifies_without_outstanding_query() {
    let mut p = StdinAnsiParser::new();
    let out = p.feed(b"\x1b[?997;1n");
    assert!(
        out.residue.is_empty(),
        "the notification must not leak as residue"
    );
    assert_eq!(out.replies.len(), 1);
    match &out.replies[0] {
        HostReply::HostTerminalThemeChanged(mode) => {
            assert_eq!(*mode, zellij_utils::data::HostTerminalThemeMode::Dark);
        },
        other => panic!("expected HostTerminalThemeChanged, got {:?}", other),
    }
}

fn announce_frame_and_payload() -> (Vec<u8>, Vec<u8>) {
    use zellij_utils::nested_session::{
        encode_frame, encode_payload, NestedSessionCapability, NestedSessionMessage,
    };
    let message = NestedSessionMessage::Announce {
        session_name: "guest-session".to_owned(),
        capabilities: vec![NestedSessionCapability::NestedControl],
    };
    (encode_frame(&message), encode_payload(&message))
}

#[test]
fn nested_frame_in_one_chunk_is_extracted_with_no_residue() {
    let (frame, payload) = announce_frame_and_payload();
    let mut p = StdinAnsiParser::new();
    let out = p.feed(&frame);
    assert_eq!(out.nested_frames, vec![payload]);
    assert!(out.residue.is_empty(), "residue: {:?}", out.residue);
    assert_eq!(p.pending_partial(), PendingPartial::None);
}

#[test]
fn nested_frame_split_at_every_boundary_is_extracted() {
    let (frame, payload) = announce_frame_and_payload();
    for split_at in 1..frame.len() {
        let mut p = StdinAnsiParser::new();
        let first = p.feed(&frame[..split_at]);
        let second = p.feed(&frame[split_at..]);
        let mut extracted = first.nested_frames.clone();
        extracted.extend(second.nested_frames.clone());
        assert_eq!(extracted, vec![payload.clone()], "split at {}", split_at);
        assert!(
            first.residue.is_empty() && second.residue.is_empty(),
            "split at {}: residue {:?} / {:?}",
            split_at,
            first.residue,
            second.residue
        );
    }
}

#[test]
fn keystrokes_around_nested_frame_survive_as_residue() {
    let (frame, payload) = announce_frame_and_payload();
    let mut chunk = b"abc".to_vec();
    chunk.extend_from_slice(&frame);
    chunk.extend_from_slice(b"def");
    let mut p = StdinAnsiParser::new();
    let out = p.feed(&chunk);
    assert_eq!(out.nested_frames, vec![payload]);
    assert_eq!(out.residue, b"abcdef".to_vec());
}

#[test]
fn nested_frame_with_garbage_base64_is_stripped_without_payload() {
    let mut p = StdinAnsiParser::new();
    let out = p.feed(b"\x1bP26661n!!!not-base64!!!\x1b\\");
    assert!(out.nested_frames.is_empty());
    assert!(out.residue.is_empty(), "residue: {:?}", out.residue);
}

#[test]
fn foreign_dcs_passes_through_untouched() {
    let foreign_dcs = b"\x1bP1$qm\x1b\\q".to_vec();
    let mut p = StdinAnsiParser::new();
    let out = p.feed(&foreign_dcs);
    assert!(out.nested_frames.is_empty());
    assert_eq!(out.residue, foreign_dcs);
}

#[test]
fn lone_esc_claimed_by_frame_prepass_reroutes_on_next_chunk() {
    let mut p = StdinAnsiParser::new();
    let first = p.feed(b"\x1b");
    assert!(first.residue.is_empty());
    assert!(first.has_partial_state);
    assert_eq!(p.pending_partial(), PendingPartial::LoneEsc);
    let second = p.feed(b"abc");
    assert_eq!(second.residue, b"\x1babc".to_vec());
    assert_eq!(p.pending_partial(), PendingPartial::None);
}

#[test]
fn unterminated_frame_header_prefix_is_released_by_force_drain() {
    let mut p = StdinAnsiParser::new();
    let out = p.feed(b"\x1bP26");
    assert!(out.residue.is_empty());
    assert!(out.has_partial_state);
    assert_eq!(p.pending_partial(), PendingPartial::ReplyInProgress);
    assert_eq!(p.finalize_force(), b"\x1bP26".to_vec());
    assert_eq!(p.pending_partial(), PendingPartial::None);
}

#[test]
fn two_nested_frames_in_one_chunk_are_both_extracted() {
    let (frame, payload) = announce_frame_and_payload();
    let mut chunk = frame.clone();
    chunk.extend_from_slice(&frame);
    let mut p = StdinAnsiParser::new();
    let out = p.feed(&chunk);
    assert_eq!(out.nested_frames, vec![payload.clone(), payload]);
    assert!(out.residue.is_empty());
}

#[test]
fn clipboard_forward_is_closed_by_the_hosts_osc_52_reply() {
    let mut parser = StdinAnsiParser::new();
    parser.open_clipboard_forward(9);
    let out = parser.feed(b"\x1b]52;c;aGVsbG8=\x1b\\");
    assert_eq!(
        out.completed_clipboard_forward,
        Some((9, b"\x1b]52;c;aGVsbG8=\x1b\\".to_vec())),
        "the host's reply must be relayed verbatim (re-serialized with ST)"
    );
    assert!(out.residue.is_empty(), "clipboard data must never be typed");
    assert!(parser.active_clipboard_forward_token().is_none());
}

#[test]
fn clipboard_forward_is_closed_by_the_first_key_press() {
    let mut parser = StdinAnsiParser::new();
    parser.open_clipboard_forward(4);
    let out = parser.feed(b"a");
    assert_eq!(out.completed_clipboard_forward, Some((4, Vec::new())));
    assert_eq!(out.residue, b"a", "the key itself must still reach the app");
    assert!(parser.active_clipboard_forward_token().is_none());
}

#[test]
fn clipboard_forward_survives_host_reports_and_resizes() {
    let mut parser = StdinAnsiParser::new();
    parser.open_clipboard_forward(11);
    let out = parser.feed(b"\x1b[4;720;1280t");
    assert!(out.completed_clipboard_forward.is_none());
    let out = parser.feed(b"\x1b]11;rgb:0000/0000/0000\x1b\\");
    assert!(out.completed_clipboard_forward.is_none());
    let out = parser.feed(b"\x1b[?62;52c");
    assert!(
        out.completed_clipboard_forward.is_none(),
        "the Primary-DA barrier must not close a clipboard window"
    );
    assert_eq!(parser.active_clipboard_forward_token(), Some(11));
}

#[test]
fn a_reply_arriving_before_the_keypress_wins() {
    let mut parser = StdinAnsiParser::new();
    parser.open_clipboard_forward(2);
    let mut input = Vec::new();
    input.extend_from_slice(b"\x1b]52;c;d29ybGQ=\x1b\\");
    input.extend_from_slice(b"x");
    let out = parser.feed(&input);
    assert_eq!(
        out.completed_clipboard_forward,
        Some((2, b"\x1b]52;c;d29ybGQ=\x1b\\".to_vec()))
    );
    assert_eq!(out.residue, b"x");
}

#[test]
fn clipboard_and_barrier_forwards_are_independent() {
    let mut parser = StdinAnsiParser::new();
    parser.open_clipboard_forward(1);
    parser.open_forward(2);
    let out = parser.feed(b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\\x1b[?62;52c");
    assert_eq!(
        out.completed_forward,
        Some((2, b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\".to_vec()))
    );
    assert!(out.completed_clipboard_forward.is_none());
    assert_eq!(parser.active_clipboard_forward_token(), Some(1));
}

#[test]
fn clipboard_reply_is_not_accumulated_into_a_barrier_slot() {
    let mut parser = StdinAnsiParser::new();
    parser.open_clipboard_forward(1);
    parser.open_forward(2);
    let out = parser.feed(b"\x1b]52;c;aGk=\x1b\\");
    assert_eq!(
        out.completed_clipboard_forward,
        Some((1, b"\x1b]52;c;aGk=\x1b\\".to_vec()))
    );
    let out = parser.feed(b"\x1b[?62;52c");
    assert_eq!(
        out.completed_forward,
        Some((2, Vec::new())),
        "the clipboard payload must not leak into the barrier slot"
    );
}

#[test]
fn taking_the_clipboard_slot_hands_the_token_over() {
    let mut parser = StdinAnsiParser::new();
    parser.open_clipboard_forward(6);
    assert_eq!(
        parser.take_active_clipboard_forward(),
        Some((6, Vec::new()))
    );
    assert!(parser.active_clipboard_forward_token().is_none());
    assert_eq!(parser.take_active_clipboard_forward(), None);
}

#[test]
fn clipboard_timeout_only_fires_for_the_open_token() {
    let mut parser = StdinAnsiParser::new();
    parser.open_clipboard_forward(3);
    assert_eq!(parser.close_clipboard_forward_on_timeout(4), None);
    assert_eq!(
        parser.close_clipboard_forward_on_timeout(3),
        Some((3, Vec::new()))
    );
    assert_eq!(parser.close_clipboard_forward_on_timeout(3), None);
}

#[test]
fn outstanding_queries_do_not_consume_literal_text_or_escape_chords() {
    let mut parser = StdinAnsiParser::new();
    parser.expect_host_replies();
    assert_eq!(parser.feed(b"6;17;7t").residue, b"6;17;7t");
    assert_eq!(parser.feed(b"\x1ba").residue, b"\x1ba");
    assert_eq!(parser.pending_partial(), PendingPartial::None);
}

#[test]
fn each_outstanding_query_requires_its_own_boundary() {
    let mut parser = StdinAnsiParser::new();
    parser.expect_host_replies();
    parser.expect_host_replies();
    parser.feed(b"\x1b[?62;4c");

    parser.feed(b"\x1b");
    assert_eq!(parser.pending_partial(), PendingPartial::ExpectedReply);
    assert!(parser.finalize_fast_partial().is_empty());
    assert_eq!(parser.feed(b"a").residue, b"\x1ba");

    parser.feed(b"\x1b[?62;4c");
    parser.feed(b"\x1b");
    assert_eq!(parser.pending_partial(), PendingPartial::LoneEsc);
    assert_eq!(parser.finalize_fast_partial(), b"\x1b");
}

#[test]
fn a_silent_query_eventually_releases_genuine_escape_and_alt_brackets() {
    for prefix in [b"\x1b".as_ref(), b"\x1b[".as_ref(), b"\x1b]".as_ref()] {
        let mut parser = StdinAnsiParser::new();
        parser.expect_host_replies();
        parser.feed(prefix);
        assert_eq!(parser.pending_partial(), PendingPartial::ExpectedReply);
        assert!(parser.finalize_fast_partial().is_empty());

        // Drive the deadline without sleeping or relying on scheduler timing.
        parser.host_reply_deadline = Some(std::time::Instant::now() - Duration::from_secs(1));
        let drained = parser.finalize_fast_partial();
        assert_eq!(drained, prefix);
        assert_eq!(parser.pending_partial(), PendingPartial::None);
        let expected_key = match prefix {
            b"\x1b" => InputEvent::Key(KeyEvent {
                key: KeyCode::Escape,
                modifiers: Modifiers::NONE,
            }),
            b"\x1b[" => alt_char('['),
            _ => alt_char(']'),
        };
        assert_eq!(keys_from_flushed_bytes(&drained), vec![expected_key]);
        assert_eq!(parser.feed(b"6;17;7t").residue, b"6;17;7t");
    }
}

#[test]
fn startup_replies_and_expired_forwards_cannot_leak_into_a_new_forward() {
    let mut parser = StdinAnsiParser::new();
    parser.expect_host_replies();
    assert!(parser.feed(b"\x1b[6;").residue.is_empty());

    // Fish can issue color queries while the startup cell-size reply
    // is still arriving, and retry after its first forward times out.
    parser.open_forward(1);
    assert_eq!(parser.close_forward_on_timeout(1), Some((1, Vec::new())));
    parser.open_forward(2);

    let cell_tail = parser.feed(b"17;7t");
    assert!(cell_tail.residue.is_empty());
    assert!(cell_tail.completed_forward.is_none());
    let startup_tail = parser.feed(
        b"\x1b[4;816;1120t\x1b]11;rgb:0000/0000/0000\x1b\\\
          \x1b]10;rgb:ffff/ffff/ffff\x1b\\\x1b[?2026;2$y\x1b[?1;2c",
    );
    assert!(startup_tail.residue.is_empty());
    assert!(startup_tail.completed_forward.is_none());
    assert_eq!(parser.active_forward_token(), Some(2));
    assert!(startup_tail
        .replies
        .iter()
        .any(|reply| matches!(reply, HostReply::PixelDimensions(_))));

    let expired_reply = parser.feed(b"\x1b]11;rgb:1111/1111/1111\x1b\\\x1b[?1;2c");
    assert!(expired_reply.residue.is_empty());
    assert!(expired_reply.completed_forward.is_none());
    assert_eq!(parser.active_forward_token(), Some(2));

    let current_reply = b"\x1b]11;rgb:2222/2222/2222\x1b\\";
    let current = parser.feed(current_reply);
    assert!(current.residue.is_empty());
    let boundary = parser.feed(b"\x1b[?1;2c");
    assert_eq!(
        boundary.completed_forward,
        Some((2, current_reply.to_vec())),
        "only this forward's response may reach the application's PTY"
    );
}

#[test]
fn resize_boundary_between_forwards_preserves_each_batch_owner() {
    let mut parser = StdinAnsiParser::new();
    parser.open_forward(1);
    parser.expect_host_replies(); // resize query written after forward 1
    let first = parser.feed(b"\x1b]11;rgb:1111/1111/1111\x1b\\\x1b[?1;2c");
    assert_eq!(
        first.completed_forward,
        Some((1, b"\x1b]11;rgb:1111/1111/1111\x1b\\".to_vec()))
    );

    parser.open_forward(2); // written before the resize response arrives
    let resize = parser.feed(b"\x1b[4;816;1120t\x1b[6;17;7t\x1b[?1;2c");
    assert!(resize.residue.is_empty());
    assert!(resize.completed_forward.is_none());
    assert_eq!(parser.active_forward_token(), Some(2));
    let second = parser.feed(b"\x1b]11;rgb:2222/2222/2222\x1b\\\x1b[?1;2c");
    assert_eq!(
        second.completed_forward,
        Some((2, b"\x1b]11;rgb:2222/2222/2222\x1b\\".to_vec()))
    );
}

fn kitty_clipboard_host_id(query: &[u8]) -> Vec<u8> {
    let end = query[2..].iter().position(|b| *b == 0x1b || *b == 7).unwrap() + 2;
    zellij_utils::kitty_clipboard::Packet::parse(&query[2..end]).unwrap().id()
}

fn kitty_clipboard_reply(metadata: &[u8], id: &[u8], terminator: &[u8]) -> Vec<u8> {
    zellij_utils::kitty_clipboard::Packet::parse(metadata).unwrap().with_id(Some(id), terminator)
}

#[test]
fn kitty_clipboard_fragmented_mime_stream_preserves_binary_and_raw_terminators() {
    let mut parser = StdinAnsiParser::new();
    let outgoing = parser.prepare_clipboard_forward(70, b"\x1b]5522;type=read:id=editor;dGV4dC9wbGFpbiBpbWFnZS9wbmc=\x1b\\".to_vec());
    let id = kitty_clipboard_host_id(&outgoing);
    assert_ne!(id, b"editor");
    // Known PNG signature (non-UTF8 binary data) and a separate text fallback.
    let packets: &[(&[u8], &[u8])] = &[
        (b"5522;type=read:status=OK", b"\x1b\\"),
        (b"5522;type=read:status=DATA:mime=aW1hZ2UvcG5n;iVBORw0KGgo=", b"\x07"),
        (b"5522;type=read:status=DATA:mime=dGV4dC9wbGFpbg==;ZmFsbGJhY2s=", b"\x1b\\"),
        (b"5522;type=read:status=DONE", b"\x1b\\"),
    ];
    let mut expected = Vec::new();
    let mut completed = None;
    for (payload, terminator) in packets {
        expected.extend_from_slice(&kitty_clipboard_reply(payload, b"editor", terminator));
        for byte in kitty_clipboard_reply(payload, &id, terminator) {
            let out = parser.feed(&[byte]);
            assert!(out.residue.is_empty());
            if let Some(reply) = out.completed_clipboard_forward {
                assert!(completed.is_none());
                completed = Some(reply);
            }
        }
    }
    assert_eq!(completed, Some((70, expected)));
    assert_eq!(parser.active_clipboard_forward_token(), None);
}

#[test]
fn kitty_clipboard_large_stream_is_not_completed_by_da_or_keyboard_input() {
    let mut parser = StdinAnsiParser::new();
    let query = parser.prepare_clipboard_forward(71, b"\x1b]5522;type=read;aW1hZ2UvcG5n\x1b\\".to_vec());
    let id = kitty_clipboard_host_id(&query);
    assert!(parser.feed(&kitty_clipboard_reply(b"5522;type=read:status=OK", &id, b"\x1b\\")).completed_clipboard_forward.is_none());
    assert_eq!(parser.feed(b"\x1ba hello\x1b[?62;4c").residue, b"\x1ba hello");
    assert!(parser.feed(b"\x1b]52;c;dGV4dA==\x1b\\").completed_clipboard_forward.is_none());
    assert_eq!(parser.active_clipboard_forward_token(), Some(71));
    let payload = [b"5522;type=read:status=DATA:mime=aW1hZ2UvcG5n;".as_slice(), &vec![b'A'; 5460], b"AA=="].concat();
    let data = kitty_clipboard_reply(&payload, &id, b"\x1b\\");
    for _ in 0..400 {
        for part in data.chunks(997) {
            let out = parser.feed(part);
            assert!(out.residue.is_empty());
            assert!(out.completed_clipboard_forward.is_none());
        }
    }
    let out = parser.feed(&kitty_clipboard_reply(b"5522;type=read:status=DONE", &id, b"\x1b\\"));
    let (token, bytes) = out.completed_clipboard_forward.unwrap();
    assert_eq!(token, 71);
    let restored = zellij_utils::kitty_clipboard::Packet::parse(&payload).unwrap().with_id(None, b"\x1b\\");
    let expected = [
        b"\x1b]5522;type=read:status=OK\x1b\\".to_vec(),
        restored.repeat(400),
        b"\x1b]5522;type=read:status=DONE\x1b\\".to_vec(),
    ].concat();
    assert_eq!(bytes, expected);
}

#[test]
fn kitty_clipboard_permissions_and_missing_mime_are_not_fabricated_as_text() {
    for status in [b"EPERM".as_slice(), b"ENOSYS", b"EBUSY"] {
        let mut parser = StdinAnsiParser::new();
        let query = parser.prepare_clipboard_forward(72, b"\x1b]5522;type=read:id=app;Lg==\x1b\\".to_vec());
        let id = kitty_clipboard_host_id(&query);
        let metadata = [b"5522;type=read:status=".as_slice(), status].concat();
        let expected = kitty_clipboard_reply(&metadata, b"app", b"\x1b\\");
        let out = parser.feed(&kitty_clipboard_reply(&metadata, &id, b"\x1b\\"));
        assert_eq!(out.completed_clipboard_forward, Some((72, expected)));
        assert!(out.residue.is_empty());
    }
}

#[test]
fn kitty_clipboard_expired_generation_and_other_clients_cannot_supply_contents() {
    let mut first_client = StdinAnsiParser::new();
    let query = b"\x1b]5522;type=read:id=reused;Lg==\x1b\\".to_vec();
    let old = kitty_clipboard_host_id(&first_client.prepare_clipboard_forward(1, query.clone()));
    first_client.feed(&kitty_clipboard_reply(b"5522;type=read:status=OK", &old, b"\x1b\\"));
    first_client.feed(b"\x1b]5522;type=read:status=DATA:mime=aW1hZ2UvcG5n;");
    assert_eq!(first_client.close_clipboard_forward_on_timeout(1), Some((1, Vec::new())));
    let new = kitty_clipboard_host_id(&first_client.prepare_clipboard_forward(2, query.clone()));
    let mut second_client = StdinAnsiParser::new();
    let other = kitty_clipboard_host_id(&second_client.prepare_clipboard_forward(2, query));
    for wrong_id in [&old, &other] {
        let out = first_client.feed(&kitty_clipboard_reply(b"5522;type=read:status=EPERM", wrong_id, b"\x1b\\"));
        assert!(out.completed_clipboard_forward.is_none());
        assert!(out.residue.is_empty());
        assert_eq!(first_client.active_clipboard_forward_token(), Some(2));
    }
    let out = first_client.feed(&kitty_clipboard_reply(b"5522;type=read:status=EPERM", &new, b"\x1b\\"));
    assert_eq!(out.completed_clipboard_forward, Some((2, kitty_clipboard_reply(b"5522;type=read:status=EPERM", b"reused", b"\x1b\\"))));
    assert_eq!(second_client.active_clipboard_forward_token(), Some(2));
}

#[test]
fn kitty_clipboard_paste_and_malformed_packets_cannot_complete_a_transfer() {
    let mut parser = StdinAnsiParser::new();
    let query = parser.prepare_clipboard_forward(3, b"\x1b]5522;type=read;Lg==\x1b\\".to_vec());
    let id = kitty_clipboard_host_id(&query);
    let fake = kitty_clipboard_reply(b"5522;type=read:status=EPERM", &id, b"\x1b\\");
    let paste = [b"\x1b[200~".as_slice(), &fake, b"\x1b[201~"].concat();
    let out = parser.feed(&paste);
    assert_eq!(out.residue, paste);
    assert!(out.completed_clipboard_forward.is_none());
    let out = parser.feed(b"\x1b]5522;type=read:id=x\x1baKEEP");
    assert!(out.residue.ends_with(b"\x1baKEEP"));
    assert!(out.completed_clipboard_forward.is_none());
    parser.feed(b"\x1b");
    assert_eq!(parser.finalize_fast_partial(), b"\x1b");
    assert_eq!(parser.active_clipboard_forward_token(), Some(3));
    assert_eq!(parser.close_clipboard_forward_on_timeout(3), Some((3, Vec::new())));
}

#[test]
fn kitty_clipboard_write_rewrites_every_packet_and_forwards_host_errors() {
    let mut parser = StdinAnsiParser::new();
    let query = b"\x1b]5522;type=write:id=writer:pw=cHc=:name=RWRpdG9y\x1b\\\
        \x1b]5522;type=wdata:mime=aW1hZ2UvcG5n;iVBORw==\x07\
        \x1b]5522;type=walias:mime=aW1hZ2UvcG5n;aW1hZ2UveC1wbmc=\x1b\\\
        \x1b]5522;type=wdata\x1b\\".to_vec();
    let outgoing = parser.prepare_clipboard_forward(4, query);
    let id = kitty_clipboard_host_id(&outgoing);
    let id_field = [b":id=".as_slice(), &id].concat();
    assert_eq!(outgoing.windows(id_field.len()).filter(|window| *window == id_field.as_slice()).count(), 4);
    let out = parser.feed(&kitty_clipboard_reply(b"5522;type=write:status=EINVAL", &id, b"\x07"));
    assert_eq!(out.completed_clipboard_forward, Some((4, kitty_clipboard_reply(b"5522;type=write:status=EINVAL", b"writer", b"\x07"))));
}

#[test]
fn kitty_clipboard_oversized_and_unfinished_host_packets_never_become_keystrokes() {
    let mut parser = StdinAnsiParser::new();
    let query = parser.prepare_clipboard_forward(10, b"\x1b]5522;type=read;Lg==\x1b\\".to_vec());
    let id = kitty_clipboard_host_id(&query);
    parser.set_clipboard_paste_context(Some(1));
    parser.feed(b"\x1b]5522;type=read:status=OK\x1b\\");
    let mut hostile = b"\x1b]5522;type=read:status=DATA:mime=Lg==;".to_vec();
    hostile.extend(std::iter::repeat_n(b'A', 32 * 1024));
    for part in hostile.chunks(1024) {
        let out = parser.feed(part);
        assert!(out.residue.is_empty());
        assert!(out.completed_clipboard_forward.is_none());
    }
    assert!(parser.partial_osc.len() <= zellij_utils::kitty_clipboard::MAX_PACKET_BYTES + 4);
    let out = parser.feed(b"\x1b\\typing");
    assert_eq!(out.residue, b"typing");
    assert_eq!(parser.active_clipboard_forward_token(), Some(10));
    parser.feed(b"\x1b]5522;type=read:status=DATA;");
    assert!(parser.finalize_force().is_empty());
    let out = parser.feed(&kitty_clipboard_reply(b"5522;type=read:status=EBUSY", &id, b"\x1b\\"));
    assert!(out.residue.is_empty());
    assert!(out.completed_clipboard_forward.is_some());
}

#[test]
fn kitty_clipboard_discard_survives_idle_timeout_and_split_terminators() {
    for oversized in [false, true] {
        for terminator in [b"\x1b\\".as_slice(), b"\x07", b"\x18", b"\x1a"] {
            let mut parser = StdinAnsiParser::new();
            let query = parser.prepare_clipboard_forward(90, b"\x1b]5522;type=read:id=app;Lg==\x1b\\".to_vec());
            let id = kitty_clipboard_host_id(&query);
            let mut partial = [b"\x1b]5522;type=read:status=DATA:id=".as_slice(), &id, b":mime=Lg==;"].concat();
            if oversized { partial.extend(std::iter::repeat_n(b'A', 32768)); }
            let out = parser.feed(&partial);
            assert!(out.residue.is_empty());
            if oversized {
                assert_eq!(out.completed_clipboard_forward,
                    Some((90, kitty_clipboard_reply(b"5522;type=read:status=EBUSY", b"app", b"\x1b\\"))));
            } else {
                assert!(out.completed_clipboard_forward.is_none());
                assert_eq!(parser.close_clipboard_forward_on_timeout(90), Some((90, Vec::new())));
            }
            assert!(parser.finalize_force().is_empty());
            assert!(parser.finalize_force().is_empty(), "repeated one-second idle ticks cannot reset wire framing");
            assert!(parser.feed(b"QUFBQkJC").residue.is_empty());
            assert!(parser.feed(&terminator[..1]).residue.is_empty());
            assert!(parser.finalize_force().is_empty());
            let out = parser.feed(&[&terminator[1..], b"typing"].concat());
            assert_eq!(out.residue, b"typing");
            assert!(out.completed_clipboard_forward.is_none());
            assert!(parser.feed(&kitty_clipboard_reply(b"5522;type=read:status=DONE", &id, b"\x1b\\")).completed_clipboard_forward.is_none());
        }
    }
}

#[test]
fn kitty_clipboard_rejected_data_aborts_instead_of_completing_a_truncated_read() {
    for bad_kind in 0..3 {
        let mut parser = StdinAnsiParser::new();
        let query = parser.prepare_clipboard_forward(91, b"\x1b]5522;type=read:id=app;dGV4dC9wbGFpbg==\x1b\\".to_vec());
        let id = kitty_clipboard_host_id(&query);
        parser.feed(&kitty_clipboard_reply(b"5522;type=read:status=OK", &id, b"\x1b\\"));
        parser.feed(&kitty_clipboard_reply(b"5522;type=read:status=DATA:mime=dGV4dC9wbGFpbg==;QUFB", &id, b"\x1b\\"));
        let mut bad = b"\x1b]5522;type=read:status=DATA".to_vec();
        if bad_kind != 2 {
            bad.extend_from_slice(b":id=");
            bad.extend_from_slice(&id);
        }
        if bad_kind == 1 { bad.extend_from_slice(b":id=duplicate"); }
        bad.extend_from_slice(b":mime=dGV4dC9wbGFpbg==;");
        if bad_kind == 0 { bad.extend(std::iter::repeat_n(b'A', 16384)); }
        else { bad.extend_from_slice(b"Q0ND"); }
        bad.extend_from_slice(b"\x1b\\");
        let out = parser.feed(&bad);
        assert!(out.residue.is_empty());
        assert_eq!(out.completed_clipboard_forward,
            Some((91, kitty_clipboard_reply(b"5522;type=read:status=EBUSY", b"app", b"\x1b\\"))));
        let tail = [
            kitty_clipboard_reply(b"5522;type=read:status=DATA:mime=dGV4dC9wbGFpbg==;QkJC", &id, b"\x1b\\"),
            kitty_clipboard_reply(b"5522;type=read:status=DONE", &id, b"\x1b\\"),
        ].concat();
        let out = parser.feed(&tail);
        assert!(out.residue.is_empty());
        assert!(out.completed_clipboard_forward.is_none(), "DONE must not deliver the surviving AAABBB bytes");
    }
}

#[test]
fn kitty_clipboard_oversized_stale_generation_does_not_abort_the_current_read() {
    let mut parser = StdinAnsiParser::new();
    parser.prepare_clipboard_forward(1, b"\x1b]5522;type=read;Lg==\x1b\\".to_vec());
    let old = parser.active_clipboard_forward.as_ref().unwrap().kitty.as_ref().unwrap().host_id.clone();
    parser.close_clipboard_forward_on_timeout(1);
    let query = parser.prepare_clipboard_forward(2, b"\x1b]5522;type=read;Lg==\x1b\\".to_vec());
    let current = kitty_clipboard_host_id(&query);
    let mut stale = [b"\x1b]5522;type=read:status=DATA:id=".as_slice(), &old, b":mime=Lg==;"].concat();
    stale.extend(std::iter::repeat_n(b'A', 16384));
    stale.extend_from_slice(b"\x1b\\");
    assert!(parser.feed(&stale).completed_clipboard_forward.is_none());
    assert_eq!(parser.active_clipboard_forward_token(), Some(2));
    assert!(parser.feed(&kitty_clipboard_reply(b"5522;type=read:status=EPERM", &current, b"\x1b\\")).completed_clipboard_forward.is_some());
}

const KITTY_PRIMARY_PASTE: &[u8] = b"\x1b]5522;type=read:status=OK:loc=primary:pw=c2VjcmV0\x1b\\\
    \x1b]5522;type=read:status=DATA:mime=Lg==;dGV4dC9wbGFpbiBpbWFnZS9wbmc=\x07\
    \x1b]5522;type=read:status=DONE:pw=c2VjcmV0\x1b\\";

#[test]
fn kitty_clipboard_paste_stream_retains_original_context_across_focus_handoffs() {
    let mut parser = StdinAnsiParser::new();
    assert_eq!(parser.set_clipboard_paste_context(Some(100)), b"\x1b[?5522h");
    let opening = b"\x1b]5522;type=read:status=OK:loc=primary:pw=c2VjcmV0\x1b\\";
    assert!(parser.feed(opening).clipboard_pastes.is_empty());
    parser.set_clipboard_paste_context(Some(200));
    let mut events = Vec::new();
    for byte in &KITTY_PRIMARY_PASTE[opening.len()..] {
        let out = parser.feed(&[*byte]);
        assert!(out.residue.is_empty());
        assert!(out.completed_clipboard_forward.is_none());
        events.extend(out.clipboard_pastes);
    }
    assert_eq!(events, vec![(100, KITTY_PRIMARY_PASTE.to_vec())]);
    assert_eq!(parser.feed(KITTY_PRIMARY_PASTE).clipboard_pastes, vec![(200, KITTY_PRIMARY_PASTE.to_vec())]);
    assert_eq!(parser.set_clipboard_paste_context(None), b"\x1b[?5522l");
    let out = parser.feed(KITTY_PRIMARY_PASTE);
    assert!(out.clipboard_pastes.is_empty());
    assert!(out.residue.is_empty());
}

#[test]
fn kitty_clipboard_paste_tokens_remain_separate_from_solicited_image_contents() {
    let mut parser = StdinAnsiParser::new();
    parser.set_clipboard_paste_context(Some(11));
    let query = parser.prepare_clipboard_forward(92, b"\x1b]5522;type=read:id=editor;Lg==\x1b\\".to_vec());
    let id = kitty_clipboard_host_id(&query);
    let out = parser.feed(KITTY_PRIMARY_PASTE);
    assert_eq!(out.clipboard_pastes, vec![(11, KITTY_PRIMARY_PASTE.to_vec())]);
    assert!(out.completed_clipboard_forward.is_none());
    assert_eq!(parser.active_clipboard_forward_token(), Some(92));
    let out = parser.feed(&kitty_clipboard_reply(b"5522;type=read:status=EPERM", &id, b"\x1b\\"));
    assert!(out.clipboard_pastes.is_empty());
    assert!(out.completed_clipboard_forward.is_some());
    let followup = parser.prepare_clipboard_forward(93,
        b"\x1b]5522;type=read:loc=primary:pw=c2VjcmV0:name=UGFzdGUgZXZlbnQ=:id=editor;aW1hZ2UvcG5n\x1b\\".to_vec());
    assert!(followup.windows(b"loc=primary:pw=c2VjcmV0:name=UGFzdGUgZXZlbnQ=".len())
        .any(|window| window == b"loc=primary:pw=c2VjcmV0:name=UGFzdGUgZXZlbnQ="));
    assert!(followup.ends_with(b";aW1hZ2UvcG5n\x1b\\"));
}

#[test]
fn kitty_clipboard_host_mode_cache_changes_context_without_repeating_decset() {
    let mut parser = StdinAnsiParser::new();
    assert_eq!(parser.set_clipboard_paste_context(None), b"\x1b[?5522l");
    assert!(parser.set_clipboard_paste_context(None).is_empty());
    assert_eq!(parser.set_clipboard_paste_context(Some(1)), b"\x1b[?5522h");
    assert!(parser.set_clipboard_paste_context(Some(2)).is_empty());
    assert_eq!(parser.feed(KITTY_PRIMARY_PASTE).clipboard_pastes, vec![(2, KITTY_PRIMARY_PASTE.to_vec())]);
    assert_eq!(parser.set_clipboard_paste_context(None), b"\x1b[?5522l");
}

#[test]
fn kitty_clipboard_unsolicited_fragments_use_a_bounded_reply_deadline_not_keyboard_idle() {
    let mut parser = StdinAnsiParser::new();
    parser.set_clipboard_paste_context(Some(3));
    parser.feed(b"\x1b]5522;type=read:status=OK:pw=c2VjcmV0\x1b\\");
    parser.feed(b"\x1b]5522;type=read:status=DATA:mime=Lg==;");
    assert_eq!(parser.pending_partial(), PendingPartial::ExpectedReply);
    assert!(parser.finalize_fast_partial().is_empty());
    parser.partial_kitty_deadline = Some(std::time::Instant::now() - Duration::from_secs(1));
    assert_eq!(parser.pending_partial(), PendingPartial::ReplyInProgress);
    assert!(parser.finalize_force().is_empty());
    assert!(parser.feed(b"dGV4dC9wbGFpbgo=").residue.is_empty());
    let out = parser.feed(b"\x1b\\\x1b]5522;type=read:status=DONE\x1b\\typing");
    assert!(out.clipboard_pastes.is_empty(), "a timed-out MIME list must not create a partial authorization");
    assert_eq!(out.residue, b"typing");
}

#[test]
fn kitty_clipboard_and_late_osc52_bytes_cannot_leak_into_another_panes_color_query() {
    let mut parser = StdinAnsiParser::new();
    parser.prepare_clipboard_forward(1, b"\x1b]5522;type=read;Lg==\x1b\\".to_vec());
    parser.open_forward(2);
    let out = parser.feed(b"\x1b]52;c;c2VjcmV0\x1b\\\x1b]11;rgb:0000/0000/0000\x1b\\\x1b[?62;4c");
    assert_eq!(out.completed_forward, Some((2, b"\x1b]11;rgb:0000/0000/0000\x1b\\".to_vec())));
    assert!(out.completed_clipboard_forward.is_none());
    assert_eq!(parser.active_clipboard_forward_token(), Some(1));
    assert!(out.residue.is_empty());
}

#[test]
fn kitty_clipboard_every_identifiable_header_prefix_survives_idle_and_query_timeout() {
    let payload = b"5522;type=read:status=DATA:mime=Lg==;dGV4dC9wbGFpbgo=";
    for timed_out in [false, true] {
        for terminator in [b"\x1b\\".as_slice(), b"\x07", b"\x18", b"\x1a"] {
            let template = kitty_clipboard_reply(payload, &[b'a'; 32], terminator);
            // ESC and ESC ] alone remain keyboard ambiguities. Every later
            // split, including the OSC number and each metadata byte, is owned.
            for cutoff in 3..=template.len() - terminator.len() {
                let mut parser = StdinAnsiParser::new();
                let query = parser.prepare_clipboard_forward(101, b"\x1b]5522;type=read:id=app;Lg==\x1b\\".to_vec());
                let id = kitty_clipboard_host_id(&query);
                parser.feed(&kitty_clipboard_reply(b"5522;type=read:status=OK", &id, b"\x1b\\"));
                let packet = kitty_clipboard_reply(payload, &id, terminator);
                let out = parser.feed(&packet[..cutoff]);
                assert!(out.residue.is_empty(), "prefix {cutoff}");
                assert!(out.completed_clipboard_forward.is_none());
                assert_eq!(parser.pending_partial(), PendingPartial::ExpectedReply, "prefix {cutoff}");
                assert!(parser.finalize_fast_partial().is_empty());
                if timed_out {
                    assert_eq!(parser.close_clipboard_forward_on_timeout(101), Some((101, Vec::new())));
                }
                assert!(parser.finalize_force().is_empty(), "prefix {cutoff}");
                assert!(parser.finalize_force().is_empty(), "repeated idle at prefix {cutoff}");
                for byte in &packet[cutoff..] {
                    let out = parser.feed(&[*byte]);
                    assert!(out.residue.is_empty(), "continuation at prefix {cutoff}");
                    assert!(out.completed_clipboard_forward.is_none());
                }
                assert_eq!(parser.feed(b"typing").residue, b"typing");
                let out = parser.feed(&kitty_clipboard_reply(b"5522;type=read:status=DONE", &id, b"\x1b\\"));
                if timed_out {
                    assert!(out.completed_clipboard_forward.is_none());
                } else {
                    assert_eq!(out.completed_clipboard_forward,
                        Some((101, kitty_clipboard_reply(b"5522;type=read:status=EBUSY", b"app", b"\x1b\\"))));
                }
            }
        }
    }
}

#[test]
fn kitty_clipboard_header_tracking_preserves_bare_escape_alt_typing_and_bracketed_paste() {
    for timed_out in [false, true] {
        for prefix in [b"\x1b".as_slice(), b"\x1b]"] {
            let mut parser = StdinAnsiParser::new();
            parser.prepare_clipboard_forward(102, b"\x1b]5522;type=read;Lg==\x1b\\".to_vec());
            assert!(parser.feed(prefix).residue.is_empty());
            if timed_out { parser.close_clipboard_forward_on_timeout(102); }
            assert_eq!(parser.finalize_fast_partial(), prefix);
            assert_eq!(parser.feed(b"5522;literal \x1ba").residue, b"5522;literal \x1ba");
            let paste = b"\x1b[200~\x1b]5522;type=read:status=OK\x1b\\5522;literal\x1b[201~";
            let out = parser.feed(paste);
            assert_eq!(out.residue, paste);
            assert!(out.completed_clipboard_forward.is_none());
            assert!(out.clipboard_pastes.is_empty());
        }
    }
}

#[test]
fn kitty_clipboard_header_candidate_disambiguates_osc52_and_preserves_new_escape_chords() {
    let mut parser = StdinAnsiParser::new();
    parser.prepare_clipboard_forward(103, b"\x1b]5522;type=read;Lg==\x1b\\".to_vec());
    parser.feed(b"\x1b]5");
    assert_eq!(parser.pending_partial(), PendingPartial::ExpectedReply);
    let out = parser.feed(b"2;c;c2VjcmV0\x1b\\typing");
    assert_eq!(out.residue, b"typing");
    assert!(out.completed_clipboard_forward.is_none());
    assert_eq!(parser.active_clipboard_forward_token(), Some(103));
    parser.feed(b"\x1b]5522");
    let out = parser.feed(b"\x1baKEEP");
    assert_eq!(out.residue, b"\x1baKEEP");
    assert_eq!(out.completed_clipboard_forward,
        Some((103, b"\x1b]5522;type=read:status=EBUSY\x1b\\".to_vec())));
}

#[test]
fn kitty_clipboard_paste_recipient_is_pinned_before_every_opening_ok_frame_boundary() {
    let opening = b"\x1b]5522;type=read:status=OK:loc=primary:pw=c2VjcmV0\x1b\\";
    for initial_context in [None, Some(100)] {
        for next_context in [None, Some(200)] {
            // Includes lone ESC buffered by NestedFrameExtractor, OSC prefix,
            // every metadata byte, and the split ST before its final backslash.
            for cutoff in 1..opening.len() {
                let mut parser = StdinAnsiParser::new();
                parser.set_clipboard_paste_context(initial_context);
                let out = parser.feed(&opening[..cutoff]);
                assert!(out.residue.is_empty());
                assert!(out.clipboard_pastes.is_empty());
                parser.set_clipboard_paste_context(next_context);
                let out = parser.feed(&KITTY_PRIMARY_PASTE[cutoff..]);
                assert!(out.residue.is_empty());
                let expected = initial_context.map(|context| vec![(context, KITTY_PRIMARY_PASTE.to_vec())]).unwrap_or_default();
                assert_eq!(out.clipboard_pastes, expected, "opening cutoff {cutoff}");
                let fresh = parser.feed(KITTY_PRIMARY_PASTE);
                let expected = next_context.map(|context| vec![(context, KITTY_PRIMARY_PASTE.to_vec())]).unwrap_or_default();
                assert_eq!(fresh.clipboard_pastes, expected, "fresh event after cutoff {cutoff}");
            }
        }
    }
}

#[test]
fn kitty_clipboard_late_header_first_arrival_after_timeout_never_becomes_keyboard_input() {
    for had_request in [false, true] {
        for cutoff in [3, 4, 5, 6, 7, 15, 32, 55] {
            for terminator in [b"\x1b\\".as_slice(), b"\x07", b"\x18", b"\x1a"] {
                let mut parser = StdinAnsiParser::new();
                let id = if had_request {
                    let query = parser.prepare_clipboard_forward(104, b"\x1b]5522;type=read;Lg==\x1b\\".to_vec());
                    let id = kitty_clipboard_host_id(&query);
                    assert_eq!(parser.close_clipboard_forward_on_timeout(104), Some((104, Vec::new())));
                    id
                } else {
                    b"unowned".to_vec()
                };
                assert_eq!(parser.clipboard_paste_context, None);
                let packet = kitty_clipboard_reply(
                    b"5522;type=read:status=DATA:mime=dGV4dC9wbGFpbg==;QUFBQkJC", &id, terminator);
                let out = parser.feed(&packet[..cutoff]);
                assert!(out.residue.is_empty());
                assert_eq!(parser.pending_partial(), PendingPartial::ReplyInProgress);
                assert!(parser.finalize_force().is_empty(), "late header cutoff {cutoff}");
                assert!(parser.finalize_force().is_empty());
                for byte in &packet[cutoff..] {
                    let out = parser.feed(&[*byte]);
                    assert!(out.residue.is_empty(), "late continuation after cutoff {cutoff}");
                    assert!(out.completed_clipboard_forward.is_none());
                    assert!(out.clipboard_pastes.is_empty());
                }
                assert_eq!(parser.feed(b"typing \x1ba").residue, b"typing \x1ba");
                assert_eq!(parser.active_clipboard_forward_token(), None);
            }
        }
    }
}

#[test]
fn kitty_clipboard_idle_discard_keeps_nested_extractors_split_st_escape_off_keyboard() {
    let mut parser = StdinAnsiParser::new();
    parser.prepare_clipboard_forward(105, b"\x1b]5522;type=read;Lg==\x1b\\".to_vec());
    parser.feed(b"\x1b]5522;type=read:status=DATA;QUFB");
    parser.close_clipboard_forward_on_timeout(105);
    assert!(parser.finalize_force().is_empty());
    assert!(parser.feed(b"QkJC\x1b").residue.is_empty());
    assert_eq!(parser.nested_frame_extractor.partial_bytes(), [0x1b]);
    assert!(parser.finalize_force().is_empty());
    assert!(parser.finalize_force().is_empty());
    assert!(parser.discard_kitty_osc);
    let out = parser.feed(b"\\typing");
    assert_eq!(out.residue, b"typing");
    assert!(!parser.discard_kitty_osc);
}

#[test]
fn kitty_clipboard_replacing_a_query_does_not_let_old_buffered_rejection_abort_the_new_one() {
    for force_old_partial in [false, true] {
        let mut parser = StdinAnsiParser::new();
        parser.prepare_clipboard_forward(106, b"\x1b]5522;type=read;Lg==\x1b\\".to_vec());
        parser.feed(b"\x1b]5522;type=read:status=DATA:mime=aW1hZ2UvcG5n;");
        parser.close_clipboard_forward_on_timeout(106);
        let query = parser.prepare_clipboard_forward(107, b"\x1b]5522;type=read:id=new;Lg==\x1b\\".to_vec());
        let id = kitty_clipboard_host_id(&query);
        if force_old_partial {
            assert!(parser.finalize_force().is_empty());
            assert!(parser.feed(b"QUFB\x1b\\").residue.is_empty());
        }
        let packets: &[&[u8]] = &[
            b"5522;type=read:status=OK",
            b"5522;type=read:status=DATA:mime=Lg==;dGV4dC9wbGFpbgo=",
            b"5522;type=read:status=DONE",
        ];
        let mut expected = Vec::new();
        for (index, payload) in packets.iter().enumerate() {
            expected.extend_from_slice(&kitty_clipboard_reply(payload, b"new", b"\x1b\\"));
            let out = parser.feed(&kitty_clipboard_reply(payload, &id, b"\x1b\\"));
            assert!(out.residue.is_empty());
            if index + 1 == packets.len() {
                assert_eq!(out.completed_clipboard_forward, Some((107, expected.clone())));
            } else {
                assert!(out.completed_clipboard_forward.is_none());
                assert_eq!(parser.active_clipboard_forward_token(), Some(107));
            }
        }
    }
}

#[test]
fn osc52_overflow_retires_only_its_owner_and_never_turns_clipboard_data_into_keys() {
    let mut parser = StdinAnsiParser::new();
    parser.open_clipboard_forward(201);
    let mut oversized = b"\x1b]52;c;".to_vec();
    oversized.resize(super::PARTIAL_BUFFER_CAP_BYTES + 32, b'A');
    let out = parser.feed(&oversized);
    assert!(out.residue.is_empty());
    assert_eq!(out.completed_clipboard_forward, Some((201, Vec::new())));
    assert_eq!(parser.active_clipboard_forward_token(), None);
    drop(oversized);
    assert!(parser.finalize_force().is_empty());

    parser.open_clipboard_forward(202);
    assert!(parser.feed(b"QUFBQkJC").residue.is_empty());
    assert!(parser.feed(b"\x1b").residue.is_empty());
    assert!(parser.finalize_force().is_empty());
    assert!(parser.finalize_force().is_empty());
    let out = parser.feed(b"\\");
    assert!(out.residue.is_empty());
    assert!(out.completed_clipboard_forward.is_none());
    assert_eq!(parser.active_clipboard_forward_token(), Some(202));

    let out = parser.feed(b"\x1b]52;c;\x07");
    assert_eq!(out.completed_clipboard_forward, Some((202, b"\x1b]52;c;\x1b\\".to_vec())));
    assert!(out.residue.is_empty());
    assert_eq!(parser.feed(b"typing \x1ba").residue, b"typing \x1ba");
}

#[test]
fn osc52_idle_abandonment_finishes_without_contents_at_the_real_wire_boundary() {
    for terminator in [b"\x1b\\".as_slice(), b"\x07", b"\x18", b"\x1a"] {
        let mut parser = StdinAnsiParser::new();
        parser.open_clipboard_forward(203);
        assert!(parser.feed(b"\x1b]52;c;QUFB").residue.is_empty());
        assert!(parser.finalize_force().is_empty());
        assert!(parser.finalize_force().is_empty());
        let out = parser.feed(&[b"QkJC".as_slice(), terminator, b"typing"].concat());
        assert_eq!(out.completed_clipboard_forward, Some((203, Vec::new())));
        assert_eq!(out.residue, b"typing");
        assert_eq!(parser.active_clipboard_forward_token(), None);
    }
}

#[test]
fn osc52_late_fragments_after_timeout_cannot_complete_a_replacement_read() {
    for cutoff in 3..b"\x1b]52;c;QUFB".len() {
        for force in [false, true] {
            let mut parser = StdinAnsiParser::new();
            parser.open_clipboard_forward(204);
            let old_reply = b"\x1b]52;c;QUFBQkJC\x1b\\";
            assert!(parser.feed(&old_reply[..cutoff]).residue.is_empty());
            assert_eq!(parser.close_clipboard_forward_on_timeout(204), Some((204, Vec::new())));
            parser.open_clipboard_forward(205);
            if force { assert!(parser.finalize_force().is_empty()); }
            let out = parser.feed(&old_reply[cutoff..]);
            assert!(out.residue.is_empty());
            assert!(out.completed_clipboard_forward.is_none());
            assert_eq!(parser.active_clipboard_forward_token(), Some(205));
            let authentic = b"\x1b]52;c;bmV3\x1b\\";
            let out = parser.feed(authentic);
            assert_eq!(out.completed_clipboard_forward, Some((205, authentic.to_vec())));
            assert_eq!(parser.feed(b"typing").residue, b"typing");
        }
    }
}

#[test]
fn osc52_reply_arriving_after_timeout_without_any_owner_stays_out_of_keyboard() {
    for cutoff in 3..b"\x1b]52;c;QUFB".len() {
        for terminator in [b"\x1b\\".as_slice(), b"\x07", b"\x18", b"\x1a"] {
            let mut parser = StdinAnsiParser::new();
            parser.open_clipboard_forward(206);
            parser.close_clipboard_forward_on_timeout(206);
            let reply = [b"\x1b]52;c;QUFBQkJC".as_slice(), terminator].concat();
            assert!(parser.feed(&reply[..cutoff]).residue.is_empty());
            assert!(parser.finalize_force().is_empty());
            let out = parser.feed(&[&reply[cutoff..], b"typing"].concat());
            assert!(out.completed_clipboard_forward.is_none());
            assert!(out.clipboard_pastes.is_empty());
            assert_eq!(out.residue, b"typing");
        }
    }
}

#[test]
fn osc52_keyboard_cancellation_and_authentic_empty_replies_keep_wire_order() {
    let mut parser = StdinAnsiParser::new();
    parser.open_clipboard_forward(207);
    let out = parser.feed(b"x\x1b]52;c;c2VjcmV0\x1b\\");
    assert_eq!(out.completed_clipboard_forward, Some((207, Vec::new())));
    assert_eq!(out.residue, b"x");
    parser.open_clipboard_forward(208);
    let out = parser.feed(b"\x1b]52;c;\x07x");
    assert_eq!(out.completed_clipboard_forward, Some((208, b"\x1b]52;c;\x1b\\".to_vec())));
    assert_eq!(out.residue, b"x");
}

#[test]
fn osc52_idle_or_malformed_replies_never_cancel_an_independent_kitty_request() {
    for malformed in [false, true] {
        let mut parser = StdinAnsiParser::new();
        let query = parser.prepare_clipboard_forward(209, b"\x1b]5522;type=read:id=kitty;Lg==\x1b\\".to_vec());
        let id = kitty_clipboard_host_id(&query);
        parser.feed(b"\x1b]52;c;QUFB");
        if malformed {
            let out = parser.feed(b"\x1ba");
            assert_eq!(out.residue, b"\x1ba");
            assert!(out.completed_clipboard_forward.is_none());
        } else {
            assert!(parser.finalize_force().is_empty());
            assert!(parser.feed(b"QkJC\x1b\\").residue.is_empty());
        }
        assert_eq!(parser.active_clipboard_forward_token(), Some(209));
        let opening = kitty_clipboard_reply(b"5522;type=read:status=OK", &id, b"\x1b\\");
        let data = kitty_clipboard_reply(b"5522;type=read:status=DATA:mime=Lg==;dGV4dC9wbGFpbgo=", &id, b"\x1b\\");
        let done = kitty_clipboard_reply(b"5522;type=read:status=DONE", &id, b"\x1b\\");
        let out = parser.feed(&[opening, data, done].concat());
        let expected = [
            kitty_clipboard_reply(b"5522;type=read:status=OK", b"kitty", b"\x1b\\"),
            kitty_clipboard_reply(b"5522;type=read:status=DATA:mime=Lg==;dGV4dC9wbGFpbgo=", b"kitty", b"\x1b\\"),
            kitty_clipboard_reply(b"5522;type=read:status=DONE", b"kitty", b"\x1b\\"),
        ].concat();
        assert_eq!(out.completed_clipboard_forward, Some((209, expected)));
        assert!(out.residue.is_empty());
    }
}

#[test]
fn xtgettcap_truecolor_boolean_survives_every_split_and_negative_rgb() {
    let response = b"\x1bP0+r524742\x1b\\\x1bP1+r5463\x1b\\\x1b[?1;2c";
    for split in 0..=response.len() {
        let mut parser = StdinAnsiParser::new();
        parser.expect_host_replies();
        let mut first = parser.feed(&response[..split]);
        let second = parser.feed(&response[split..]);
        first.replies.extend(second.replies);
        assert_eq!(first.replies.iter().filter(|reply| {
            matches!(reply, HostReply::TruecolorSupport(true))
        }).count(), 1, "split {}", split);
        assert!(!first.replies.iter().any(|reply| {
            matches!(reply, HostReply::TruecolorSupport(false))
        }));
        assert!(first.residue.is_empty(), "split {}", split);
        assert!(second.residue.is_empty(), "split {}", split);
    }
}

#[test]
fn osc_rgb_and_unknown_capabilities_do_not_advertise_truecolor() {
    let mut parser = StdinAnsiParser::new();
    parser.expect_host_replies();
    let out = parser.feed(b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\\x1bP1+r436f=323536\x1b\\\x1bP0+r5463\x1b\\\x1b[?1;2c");
    assert!(!out.replies.iter().any(|reply| matches!(reply, HostReply::TruecolorSupport(_))));
    assert!(out.residue.is_empty());
}

#[test]
fn forwarded_capability_replies_retain_the_original_wire_bytes() {
    let mut parser = StdinAnsiParser::new();
    parser.open_forward(91);
    let out = parser.feed(b"\x1bP1+r5463\x1b\\\x1b[?1;2c");
    assert_eq!(out.completed_forward, Some((91, b"\x1bP1+r5463\x1b\\".to_vec())));
    assert!(out.residue.is_empty());
}
