#!/bin/sh
# Type-checks and lints the macOS build from Linux, so a change to shared
# code can be checked against platform/macos.rs without a Mac. It cannot
# link or run anything — that still needs a Mac.
#
# `cargo check` never links, but `ring`'s build script still compiles C for
# the target, and Linux has no Apple toolchain. A stand-in compiler that
# writes empty objects satisfies it.
set -eu
rustup target add aarch64-apple-darwin >/dev/null
stub=$(mktemp -d)
trap 'rm -rf "$stub"' EXIT
cat > "$stub/cc" <<'CC'
#!/bin/sh
out=""; prev=""
for a in "$@"; do [ "$prev" = "-o" ] && out="$a"; prev="$a"; done
[ -n "$out" ] && : > "$out"
exit 0
CC
cat > "$stub/ar" <<'AR'
#!/bin/sh
for a in "$@"; do case "$a" in *.a) : > "$a";; esac; done
exit 0
AR
chmod +x "$stub/cc" "$stub/ar"
CC_aarch64_apple_darwin="$stub/cc" AR_aarch64_apple_darwin="$stub/ar" \
    cargo clippy --workspace --all-targets --target aarch64-apple-darwin "$@"
