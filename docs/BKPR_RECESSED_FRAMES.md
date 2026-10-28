# Recessed pane frames

## Shared-cell spacing prototype

This experimental branch enables the spacing prototype for fresh native
processes; `BKPR_COMPACT_FRAMES=0` restores the original profile. It retains one cell at the outside and shares a single
separator cell between neighboring tiled panes. The opening reaches the content
boundary, giving the surrounding surface the full reserved margin. Circular
corners, bevels and shadows compose above application content: shadows shade
backgrounds and glyphs together, and the rounded rim crops glyphs outside the
opening. Native floating-pane clipping keeps popups above the underlying pane's
frame. Three-cell tiles retain the circular radius and smooth shadow tails;
the normal profile retains its original two-cell tiles and cache behavior.

Each side clips its image to its own portion of a shared cell in physical pixels,
including at junctions and odd pixel sizes. The neighboring pane reserves the
shared left/top cell; the other pane gains its former right/bottom frame cell as
actual PTY content. Frame geometry expands into the shared cell while content,
cursor and mouse coordinates follow native content offsets.

Shared spacing requires confirmed graphics metadata for every regular client.
Unsupported attachments, stacks, small panes, reserved headers and fullscreen
use ordinary native frame spacing. The prototype lives in `recessed_frames/compact.rs`
and `tiled_panes/recessed.rs`. Use Bkpr's local-runtime devshell and `just run`
to launch this branch; the normal devshell retains the pinned runtime.

Full frames with `ui.pane_frames.rounded_corners` enabled use pixel-rendered
edges on clients with confirmed Kitty graphics, truecolor, cell dimensions and
terminal foreground/background responses. Other clients use ordinary native
frames. Stacked frames keep their native presentation. Overlay dimming still
affects text while raster pixels retain their original colors.

```kdl
pane_frames true
pane_frame_style "full"
ui {
    pane_frames {
        rounded_corners true
    }
}
```

The surface matches Bkpr's sidebar: the reported terminal background is shifted
away from its foreground by six percent of their relative luminance difference.
The pane interior retains the terminal background. A single integer pixel
thickness is used for every edge: half the cell width, rounded up, bounded to
leave one pixel for the interior on short cells. Corners span two columns of
the top/bottom frame strip for a larger circular curve with mirrored sample
coordinates, including for odd cell dimensions. Horizontal and vertical edges
use exactly the same pixel thickness. Spare frame-row height stays
content-colored, since terminal text cannot occupy fractional rows. Opaque frame
pixels fit inside reserved frame cells; PTY geometry, input and hit testing
remain native.

## Depth effects

The surrounding surface casts an inset shadow into the opening. Its actual mask
is translated down/right by equal physical-pixel offsets and blurred with a
Gaussian kernel, producing an upper/left cast shadow rather than a uniform edge
gradient. Straight edges use the continuous Gaussian integral; corners integrate
over the circular opening with 64 samples and an analytic horizontal integral.
This avoids the visible terraces of a sparse binary-mask blur at large fonts.
Final output still has eight-bit RGBA precision. Transparent black pixels can extend
into adjacent content rows/columns, composited over both glyphs and application
backgrounds. Placements use the highest Kitty image z-index and are clipped
against native floating panes. The opaque frame and PTY geometry are unchanged.

A narrow bevel on the surrounding lip shares the same light direction: the
upper/left facet is shaded and the lower/right facet catches a fine highlight.
Normals come from the circular opening mask, so lighting continues through
corners without changing the radius or separator thickness. Colors derive from
the terminal palette. The rim is roughly one physical pixel wide at normal fonts.

The shading lives in `output/recessed_frames/depth.rs`. Every pane uses the same
material; there is no focus glow or focus-dependent variant. Each resolution
and palette is rasterized once and reused across panes and focus changes,
without timers or changes to focus behavior.

## Outer terminal background

While recessed frames are active, the native client sets the host's default
background to the sidebar surface with OSC 11. Content chunks explicitly resolve
their default background to the original queried color. Explicit application
backgrounds and pane-local OSC 11 overrides keep their existing behavior.

The client retains the exact original color response for restoration on detach,
session switch, ordinary exit and handled errors/panics. Background queries from
panes receive that original color while the host surface is owned. Palette
refreshes filter our own surface out of host replies, preventing repeated
darkening, and accept actual changed theme colors as the new baseline. An
unrecoverable kill cannot execute terminal cleanup.

The surface is sent only when it changes, as optional protobuf render metadata;
older clients ignore it and render normally. Attaching a new client to an older
server remains ordinary rendering. Unsupported clients and ordinary frames do
not change the host background. No terminal configuration files are modified.

## Integration

The implementation lives in `zellij-server/src/output/recessed_frames.rs`.
Small hooks connect terminal metadata, shared frame rendering, partial frame
repaints and final output serialization. Recessed frames clear native frame
text, including the title/CWD and bottom hover hints. Labels remain in the
sidebar; terminal titles and application content are unaffected. Cursor handling,
focus, input and floating-pane clipping remain native.
Watcher output, which does not serialize Kitty images, retains ordinary frames.
Eight edge/corner tiles share one RGBA atlas. Cache keys include cell dimensions,
background/surface colors, thickness and radius. Each key is rasterized once
and held by a reference in the native image store. Earlier resolutions and
palettes remain available when clients change font size or theme. The image
store also caches base64, while the existing per-client host state uploads each
atlas only when that client needs it. Placement updates do not rasterize or
re-encode images.

The atlas cache admits up to 16 MiB of raw pixels. Once full, new keys fall back
to ordinary frames; existing keys remain reusable. Cached images are not evicted
and regenerated during the session. Client detach removes its geometry and
capability state while retaining shared atlas entries.

Frames and content are composed after both rendering paths have finished.
Frame placements reserve the upper half of the placement UID space, and a
frame-only redraw preserves existing application image placements. Partial
repaints after popup dismissal use the same cached frame presentation. Native
frame images sit above pane content and are clipped against floating panes using the
destination client's cell dimensions.
