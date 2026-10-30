# Known issues

Rough edges seen in real use, with the evidence that produced them and
where to start.

## 1. `hwdec-current` is read too early, so the load line says `hwdec=no`

**Symptom.** Loading a 4K H.264 file printed

```
tile 0: 3840x2160 @25.000 H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10 pix=? hwdec=no ao=auto
```

— no hardware decode, and no pixel format either. A 720p clip loaded in
the same session printed `pix=videotoolbox hwdec=videotoolbox`.

**It is a reporting bug, not a decode one.** The per-tile status line for
that same 4K tile, seconds later, reads `videotoolbox`, sustains 25.0fps
on 4K, and reports `drops=0` at 5% of budget. Both lines read the *same*
mpv property, `hwdec-current` — the `FileLoaded` arm of `Tile::tick`
and `Tile::status`. So VideoToolbox does engage; the
load-time probe just runs before the decoder has initialized, and mpv
answers `no` (and `video-params/pixelformat` answers nothing, hence
`pix=?`). Bigger files seem likelier to lose the race, which fits: more
setup work between `FileLoaded` and a live decoder.

**Why it matters beyond the log.** It is the one place a person is told
which decoder they got, so it reads as a 4K file falling back to
software. Worth checking whether anything *else* consumes the load-time
probe, since a wrong `hwdec` there would mislead more than a log line.

**Where to look.** The `FileLoaded` arm of `Tile::tick` in
`crates/tessera-app/src/tile.rs`. Re-reading
the property on first frame, or on mpv's `video-reconfig`, rather than at
`FileLoaded`, is the obvious direction.

## 2. `T` doesn't float a window on Wayland (COSMIC)

**Symptom.** On COSMIC under Wayland, `T` removes the window's title bar
but the window stays in COSMIC's tiling and isn't kept above other
windows, so it isn't the picture-in-picture it is on X11 and macOS.
Nothing breaks; `T` again puts the title bar back.

**Why.** On X11 `platform::set_floating` withdraws the window and maps it
back as a dialog, which the tiling WM leaves alone, and sets it
always-on-top. Wayland has no way to retype a window, and winit 0.29 sets
no window level there, so only the decorations change.

**Where to look.** `set_floating` in
`crates/tessera-app/src/platform/linux.rs`, and ROADMAP §30.
