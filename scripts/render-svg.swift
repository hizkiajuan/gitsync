// Renders an SVG to a square PNG with AppKit, which reads SVG natively.
// Usage: swift render-svg.swift <in.svg> <out.png> <pixels>
import AppKit

let arguments = CommandLine.arguments
guard arguments.count == 4, let pixels = Int(arguments[3]) else {
    FileHandle.standardError.write("usage: render-svg <in.svg> <out.png> <pixels>\n".data(using: .utf8)!)
    exit(2)
}
guard let image = NSImage(contentsOfFile: arguments[1]) else {
    FileHandle.standardError.write("cannot read \(arguments[1])\n".data(using: .utf8)!)
    exit(1)
}
let bitmap = NSBitmapImageRep(
    bitmapDataPlanes: nil, pixelsWide: pixels, pixelsHigh: pixels, bitsPerSample: 8,
    samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
    bytesPerRow: 0, bitsPerPixel: 0)!
bitmap.size = NSSize(width: pixels, height: pixels)
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: bitmap)
image.draw(in: NSRect(x: 0, y: 0, width: pixels, height: pixels))
NSGraphicsContext.restoreGraphicsState()
try bitmap.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: arguments[2]))
