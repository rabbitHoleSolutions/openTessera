# Contributing

openTessera is maintained by one person, in spare time, on one Linux machine
and one Mac. That shapes what helps most.

## Test reports

The most useful thing you can send is a report from a setup that isn't in
the [tested table](README.md#what-has-been-tested): another distribution,
Wayland, KDE, an AMD or Intel GPU, an Intel Mac, a camera brand other than
Ubiquiti. A report that says "it works" is useful too. Open an issue with
the **Bug or test report** template. It asks for what matters:

- the OS and version, the desktop or window manager, and X11 or Wayland
- the GPU and driver, and what `tessera --version` prints (the app's
  version and which libmpv it loads)
- what you did, what you expected, and what happened
- the log, ideally with `--status-interval 5`. Add `TESSERA_MPV_LOG=v` for
  playback problems.

Look the log over before posting. It names your media folders and server
addresses. With `TESSERA_MPV_LOG=v`, mpv also prints full stream URLs, and a
Plex or Jellyfin stream URL contains your access token.

## Building and checking

```sh
cargo build --release                     # target/release/tessera
cargo test --workspace
cargo clippy --workspace --all-targets    # kept at zero warnings
./scripts/check-macos.sh                  # type-checks the macOS build from Linux
```

`check-macos.sh` catches a change to shared code that breaks
`platform/macos.rs`, without a Mac. It cannot link or run anything, so
macOS behaviour still needs testing on a Mac.

Run the app windowed while developing; a bare `tessera` with `--max` takes
every monitor:

```sh
./target/release/tessera --windowed 1400x900 --overlay ~/Videos/
```

Use a scratch `HOME` (`HOME=/tmp/tessera-dev ./target/release/tessera ...`)
to keep your real settings and queue out of it. Each config directory is its
own instance, so a scratch one can run beside your everyday one.

## Code

- **Read [CLAUDE.md](CLAUDE.md) first.** Despite the name, it is the
  architecture notes for anyone, human or not. The invariants listed there
  are the things that have caused real bugs. Most fixes so far have been
  restoring one of them.
- **Logic that can be tested without a display goes in `tessera-core`**,
  with tests. `tessera-app` is the part that needs a GPU and a window.
- **Nothing outside `crates/tessera-app/src/platform/` names an operating
  system.** A platform difference becomes a function with the same
  signature in both `linux.rs` and `macos.rs`.
- **The tree is not rustfmt-formatted.** Match the style around you, and
  don't run `cargo fmt` over the whole workspace. A reformat would bury
  every real change in noise.
- **Comments explain why.** The code leans on comments for mpv, egui and
  platform workarounds. Keep that density, and say what broke when it's a
  fix.
- **UI strings are lowercase** ("max concurrent streams").

Larger changes are best discussed in an issue first. [docs/ROADMAP.md](docs/ROADMAP.md)
records what has been built and why, and what is planned.

## License

Contributions are accepted under the project's license, GPL-3.0-or-later.
