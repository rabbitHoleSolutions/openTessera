// Regenerates every icon the app ships from one master image:
//
//   swift packaging/icons/make-icons.swift
//
// source/mosaic-1440.png is the mosaic rabbit from the Quest app's brand kit
// (tessera: media/store-assets/branding/mosaic.png), so the two apps share a
// mark. Two shapes come off it:
//
//   - tessera-N.png (and assets/window-icon-64.png): the full-bleed square,
//     for the Linux icon theme and the window icon, as before.
//   - macos/icon-N.png: Apple's app-icon grid, an 824/1024 rounded rect with
//     a transparent margin. macOS does not mask app icons, so a full-bleed
//     square sits in the Dock as a hard-edged block, bigger than its
//     neighbours.
//
// CoreGraphics rather than Pillow: it is on every Mac, and these are only
// regenerated when the art changes.
import AppKit
import CoreGraphics
import ImageIO
import UniformTypeIdentifiers

let here = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
let repo = here.deletingLastPathComponent().deletingLastPathComponent()

guard let src = CGImageSourceCreateWithURL(here.appendingPathComponent("source/mosaic-1440.png") as CFURL, nil),
      let master = CGImageSourceCreateImageAtIndex(src, 0, nil) else {
    fatalError("can't read source/mosaic-1440.png")
}

func render(_ size: Int, _ draw: (CGContext, CGFloat) -> Void) -> CGImage {
    let ctx = CGContext(data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: 0,
                        space: CGColorSpace(name: CGColorSpace.sRGB)!,
                        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    ctx.interpolationQuality = .high
    draw(ctx, CGFloat(size))
    return ctx.makeImage()!
}

func write(_ img: CGImage, _ url: URL) {
    try? FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
    let dest = CGImageDestinationCreateWithURL(url as CFURL, UTType.png.identifier as CFString, 1, nil)!
    CGImageDestinationAddImage(dest, img, nil)
    guard CGImageDestinationFinalize(dest) else { fatalError("can't write \(url.path)") }
    print("wrote \(url.path.replacingOccurrences(of: repo.path + "/", with: ""))")
}

func square(_ size: Int) -> CGImage {
    render(size) { ctx, s in ctx.draw(master, in: CGRect(x: 0, y: 0, width: s, height: s)) }
}

// Apple's grid at 1024: the body is 824 square, inset 100, corner radius
// ~185 — scaled with the canvas.
func macos(_ size: Int) -> CGImage {
    render(size) { ctx, s in
        let inset = s * 100 / 1024
        let body = CGRect(x: inset, y: inset, width: s - 2 * inset, height: s - 2 * inset)
        let radius = s * 185.4 / 1024
        ctx.addPath(CGPath(roundedRect: body, cornerWidth: radius, cornerHeight: radius, transform: nil))
        ctx.clip()
        ctx.draw(master, in: body)
    }
}

for s in [16, 24, 32, 48, 64, 128, 256, 512] {
    write(square(s), here.appendingPathComponent("tessera-\(s).png"))
}
write(square(64), repo.appendingPathComponent("assets/window-icon-64.png"))
for s in [16, 32, 64, 128, 256, 512, 1024] {
    write(macos(s), here.appendingPathComponent("macos/icon-\(s).png"))
}
