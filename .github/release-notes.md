**An early release, not 1.0.** openTessera is used every day on one Linux
machine and has been tried on a couple of Macs. This is the first build
shared outside that, to find out how it does elsewhere. Expect rough edges;
[docs/KNOWN_ISSUES.md](https://github.com/rabbitHoleSolutions/openTessera/blob/main/docs/KNOWN_ISSUES.md)
lists the ones already known. Reports of what works are as useful as
reports of what doesn't. Open an issue; the template asks for what helps.

## macOS

Download the `.dmg`, open it, and drag **Tessera** into **Applications**.
Apple Silicon, macOS 15 or newer. It carries its own copy of libmpv, so it needs nothing
else installed.

**This build isn't signed by Apple yet**, so the first time you open it,
macOS says it can't verify it and offers only *Done* or *Move to Trash*.
Choose **Done**, then go to **System Settings → Privacy & Security**,
scroll down to the message about Tessera, and click **Open Anyway**. macOS
asks once more; after that it opens normally.

If you'd rather use the terminal, this does the same thing:

```sh
xattr -dr com.apple.quarantine /Applications/Tessera.app
```

## Linux

Build from source; see the README. It takes a Rust toolchain and the
distribution's libmpv.

In a report, include what `tessera --version` (on a Mac:
`/Applications/Tessera.app/Contents/MacOS/tessera --version`) prints: the
version, and which libmpv it is using.
