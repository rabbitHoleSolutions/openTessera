# vendor/

## winit 0.29.15, patched

Used through `[patch.crates-io]` in the root `Cargo.toml`. 0.29 is the last
winit line egui 0.27 takes, and 0.29.15 is its last release, so the fix
can't come from upstream without moving egui too.

Only `src/`, `Cargo.toml`, `build.rs`, `LICENSE` and `README.md` were
copied from the crates.io package. Every change is marked `PATCH(tessera)`:

- **X11 drag and drop: a drop made before its data arrived was lost.**
  winit asks the drag source for the file list on the first `XdndPosition`
  and gets it back asynchronously. A drop within that round trip arrived
  first: winit sent `XdndFinished` (accepted) having dropped nothing, then
  emitted the late list as `HoveredFile`, so the app's "drop to play" hint
  went up after the drop and never came down. The list also outlived the
  drag, so the *next* drag skipped asking for its own data and dropped the
  old files. Now the drop waits for the data and is delivered as
  `DroppedFile` (`platform_impl/linux/x11/event_processor.rs`, `dnd.rs`),
  and `XdndEnter` starts each drag from clean state.
- **Lints off** (`src/lib.rs`, `build.rs`): cargo caps lints for a registry
  crate but not for a path dependency, so the copy printed ~350 warnings on
  every build.

To move to a newer winit, check whether its X11 `XdndDrop` handler still
gives up when the data hasn't arrived; if so, carry the patch over.
