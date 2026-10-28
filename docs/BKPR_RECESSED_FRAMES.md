# Recessed pane frames

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
content-colored, since terminal text cannot occupy fractional rows. All pixels fit inside
reserved frame cells; PTY geometry, input and hit testing remain native.

## Depth effects

The content edge has a soft inset shadow, strongest toward the top and left.
It follows the same circular distance field as the rounded opening. Its smooth
falloff reaches zero within the reserved frame cells before meeting PTY content.

The pane focused by this client also has a subtle outer halo, derived from the
actual terminal foreground and surrounding surface. It fades back to the exact
surface color at the outside of the border. Focus is native and client-specific;
mouse hover and another attached client's focus do not activate it.

The shading lives in `output/recessed_frames/depth.rs`. Focused and unfocused
atlases are separate cached variants. Switching focus reuses existing pixels
and per-client uploads; it does not rasterize on every switch or run a timer.
Resolution, terminal palette and focus appearance are part of the cache key.

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
frame images sit below text and are clipped against floating panes using the
destination client's cell dimensions.
