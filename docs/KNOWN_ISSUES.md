# Known issues

Rough edges seen in real use, with the evidence that produced them and
where to start. Neither is a regression; both were noticed on macOS but
neither looks macOS-specific.

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

## 2. Enabling audio at runtime fails with a raw libmpv error

**Symptom.** Turning audio on in the config tab (C) while a tile was
playing printed

```
tile 0: set audio failed: libmpv error -12 (COMMAND)
```

The tile stayed silent. The app had already said `audio is off — enable
it in config (C), then restart`, and `settings: audio enabled` was logged
straight after the failure, so the setting itself persisted — it just did
not reach the live tile.

**Likely cause (unverified).** `Tile::set_audio_enabled` issues
`set audio <yes|no>`. In mpv, `audio` is an alias for
`aid`, whose values are `auto`, `no`, or a track number — `yes` is not
one of them, which would explain `MPV_ERROR_COMMAND`. If that is right,
`set aid auto` / `set aid no` is the fix, and runtime audio toggling
would then work without the restart the UI currently asks for. Untested.

**Where to look.** `Tile::set_audio_enabled` in
`crates/tessera-app/src/tile.rs`, and the
"then restart" wording in the config tab if the restart stops being
necessary. Note the failure is only `eprintln!`-ed — a user who does not
watch the log sees the toggle move and nothing happen, so this wants a
`notify` toast either way.
