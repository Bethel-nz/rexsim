// Draws rexsim's button bar as a PNG: a Liquid Glass-style capsule with SF Symbols,
// like iPhone Mirroring's controls. rexsim compiles this once and caches the binary.
//   bar <width px> <height px> <pressed index or -1> <scale> <out.png>
import AppKit

let a = CommandLine.arguments
guard a.count == 6, let w = Double(a[1]), let h = Double(a[2]), let pressed = Int(a[3]), let scale = Double(a[4]) else { exit(1) }
let symbols = ["house.fill", "square.grid.3x3.fill", "lock.fill", "apple.intelligence", "speaker.wave.1.fill", "speaker.wave.3.fill"]
let fallback = ["house.fill", "square.grid.3x3.fill", "lock.fill", "waveform", "speaker.wave.1.fill", "speaker.wave.3.fill"]

let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(w), pixelsHigh: Int(h), bitsPerSample: 8, samplesPerPixel: 4,
                           hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
let ctx = NSGraphicsContext.current!.cgContext

// The capsule, inset so its shadow fits.
let inset = 3.0 * scale
let bar = CGRect(x: inset, y: inset, width: w - inset * 2, height: h - inset * 2)
let capsule = NSBezierPath(roundedRect: bar, xRadius: bar.height / 2, yRadius: bar.height / 2)

// Soft shadow under the glass.
ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: -1.5 * scale), blur: 6 * scale, color: NSColor.black.withAlphaComponent(0.35).cgColor)
NSColor(white: 0.12, alpha: 0.55).setFill()
capsule.fill()
ctx.restoreGState()

// Frosted body: a light wash, brighter at the top, like light on glass.
ctx.saveGState()
capsule.addClip()
NSGradient(colors: [NSColor(white: 1, alpha: 0.22), NSColor(white: 1, alpha: 0.08)])!.draw(in: bar, angle: -90)
ctx.restoreGState()

// Rim: bright on top, fading down, plus a faint dark hairline outside.
let rim = NSBezierPath(roundedRect: bar.insetBy(dx: 0.5 * scale, dy: 0.5 * scale), xRadius: bar.height / 2, yRadius: bar.height / 2)
rim.lineWidth = 1 * scale
NSColor(white: 1, alpha: 0.32).setStroke(); rim.stroke()
// A brighter highlight along the top edge only.
ctx.saveGState()
NSBezierPath(rect: CGRect(x: bar.minX, y: bar.midY, width: bar.width, height: bar.height / 2)).addClip()
NSColor(white: 1, alpha: 0.35).setStroke(); rim.stroke()
ctx.restoreGState()

// The buttons: equal slots across the capsule.
let n = symbols.count
let slot = (bar.width - bar.height * 0.4) / Double(n)
let x0 = bar.minX + bar.height * 0.2
let icon = bar.height * 0.42
for i in 0..<n {
    let c = CGPoint(x: x0 + slot * (Double(i) + 0.5), y: bar.midY)
    if i == pressed {
        let d = bar.height * 0.78
        NSColor(white: 1, alpha: 0.28).setFill()
        NSBezierPath(ovalIn: CGRect(x: c.x - d / 2, y: c.y - d / 2, width: d, height: d)).fill()
    }
    let cfg = NSImage.SymbolConfiguration(pointSize: icon, weight: .semibold)
        .applying(NSImage.SymbolConfiguration(paletteColors: [NSColor(white: 1, alpha: i == pressed ? 1 : 0.9)]))
    guard let img = (NSImage(systemSymbolName: symbols[i], accessibilityDescription: nil)
                     ?? NSImage(systemSymbolName: fallback[i], accessibilityDescription: nil))?.withSymbolConfiguration(cfg) else { continue }
    let s = img.size
    let k = min(icon * 1.25 / s.width, icon * 1.1 / s.height, 1.6)
    let r = CGRect(x: c.x - s.width * k / 2, y: c.y - s.height * k / 2, width: s.width * k, height: s.height * k)
    img.draw(in: r)
}
NSGraphicsContext.restoreGraphicsState()
try? rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: a[5]))
