// Makes an app the default handler for content types, and reports what it
// replaced. Used by build-app.sh --default.
//
//   swift set-default.swift /path/to/App.app public.mpeg-4 org.matroska.mkv ...
//
// NSWorkspace's API, not LaunchServices' LSSetDefaultRoleHandlerForContentType:
// on current macOS the old call still returns success but changes nothing.

import AppKit
import UniformTypeIdentifiers

let args = CommandLine.arguments.dropFirst()
guard let appPath = args.first else {
    FileHandle.standardError.write("usage: set-default.swift <App.app> <type>...\n".data(using: .utf8)!)
    exit(2)
}
let app = URL(fileURLWithPath: appPath)
let workspace = NSWorkspace.shared
var failed = false

for id in args.dropFirst() {
    guard let type = UTType(id) else {
        print(String(format: "  %-34@ unknown type", id))
        failed = true
        continue
    }
    let before = workspace.urlForApplication(toOpen: type)
        .flatMap { Bundle(url: $0)?.bundleIdentifier } ?? "none"
    // The completion arrives on the main queue, so wait by running the run
    // loop — blocking on a semaphore here would never see it. macOS may ask
    // the user to confirm; the generous timeout leaves time to answer.
    var done = false
    var error: Error?
    workspace.setDefaultApplication(at: app, toOpen: type) { e in
        error = e
        done = true
    }
    let deadline = Date().addingTimeInterval(60)
    while !done && Date() < deadline {
        RunLoop.main.run(until: Date().addingTimeInterval(0.05))
    }
    let result: String
    if !done {
        result = "timed out"
        failed = true
    } else if let e = error {
        result = "failed: \(e.localizedDescription)"
        failed = true
    } else {
        result = "ok"
    }
    print(String(format: "  %-34@ %@  [was %@]", id, result, before))
}
exit(failed ? 1 : 0)
