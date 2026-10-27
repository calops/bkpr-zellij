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
thickness is used for every edge. Corners use a circular curve with mirrored
sample coordinates, including for odd cell dimensions. All pixels fit inside
reserved frame cells; PTY geometry, input and hit testing remain native.

## Integration

The implementation lives in `zellij-server/src/output/recessed_frames.rs`.
Small hooks connect terminal metadata, shared frame rendering, partial frame
repaints and final output serialization. Existing frame titles, focus colors,
cursor handling and floating-pane clipping remain in their current paths.
Watcher output, which does not serialize Kitty images, retains ordinary frames.
Native boundary tokens are blanked inside a scoped frame-builder context;
arbitrary characters in pane titles are preserved. The context restores itself
on scope exit, including unwinding, and never wraps terminal output parsing.

Eight one-cell tiles share one RGBA atlas. Cache keys include cell dimensions,
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
