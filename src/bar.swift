// Draws rexsim's button bar as a PNG: one small glass pill per group of buttons
// (navigation, power, volume), with SF Symbols. rexsim compiles this once and caches it.
//   bar <width px> <height px> <pressed index or -1> <slots "x0:x1,..."> <groups "2,2,2"> <out.png>
// rexsim computes the slots and hit-tests clicks with the same numbers, so what's drawn
// is what's clickable.
import AppKit

let a = CommandLine.arguments
guard a.count == 7, let W = Double(a[1]), let H = Double(a[2]), let pressed = Int(a[3]) else { exit(1) }
let slots: [(Double, Double)] = a[4].split(separator: ",").compactMap {
    let p = $0.split(separator: ":").compactMap { Double($0) }
    return p.count == 2 ? (p[0], p[1]) : nil
}
let groups = a[5].split(separator: ",").compactMap { Int($0) }
let symbols = ["house.fill", "square.grid.3x3.fill", "lock.fill", "apple.intelligence", "speaker.minus.fill", "speaker.plus.fill"]
let fallback = ["house.fill", "square.grid.3x3.fill", "lock.fill", "waveform", "speaker.wave.1.fill", "speaker.wave.3.fill"]
guard slots.count == symbols.count, groups.reduce(0, +) == slots.count else { exit(1) }

let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(W), pixelsHigh: Int(H), bitsPerSample: 8, samplesPerPixel: 4,
                           hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
let ctx = NSGraphicsContext.current!.cgContext

// Room around the pills for their shadow. rexsim's layout assumes the same 4 px.
let pad = 4.0
let pillH = H - pad * 2
let radius = pillH / 2

var first = 0
for n in groups {
    let pill = CGRect(x: slots[first].0, y: pad, width: slots[first + n - 1].1 - slots[first].0, height: pillH)
    let body = NSBezierPath(roundedRect: pill, xRadius: radius, yRadius: radius)

    // A dark, mostly opaque body with a soft shadow, so it reads on any background.
    ctx.saveGState()
    ctx.setShadow(offset: CGSize(width: 0, height: -1), blur: 4, color: NSColor.black.withAlphaComponent(0.5).cgColor)
    NSColor(white: 0.09, alpha: 0.82).setFill()
    body.fill()
    ctx.restoreGState()

    // Glass sheen: a faint wash, brighter at the top.
    ctx.saveGState()
    body.addClip()
    NSGradient(colors: [NSColor(white: 1, alpha: 0.13), NSColor(white: 1, alpha: 0.02)])!.draw(in: pill, angle: -90)
    ctx.restoreGState()

    // Rim: a hairline all round, catching more light along the top.
    let rim = NSBezierPath(roundedRect: pill.insetBy(dx: 0.5, dy: 0.5), xRadius: radius - 0.5, yRadius: radius - 0.5)
    rim.lineWidth = 1
    NSColor(white: 1, alpha: 0.09).setStroke()
    rim.stroke()
    ctx.saveGState()
    NSBezierPath(rect: CGRect(x: pill.minX, y: pill.midY, width: pill.width, height: pill.height / 2 + 1)).addClip()
    NSColor(white: 1, alpha: 0.2).setStroke()
    rim.stroke()
    ctx.restoreGState()

    // A short divider between the buttons that share a pill.
    for j in first..<(first + n - 1) {
        NSColor(white: 1, alpha: 0.1).setFill()
        NSBezierPath(rect: CGRect(x: slots[j].1 - 0.5, y: pill.minY + pillH * 0.3, width: 1, height: pillH * 0.4)).fill()
    }
    first += n
}

// The icons, centred in their slots. The pressed one gets a lit disc behind it.
let icon = pillH * 0.38
for (i, s) in slots.enumerated() {
    let c = CGPoint(x: (s.0 + s.1) / 2, y: H / 2)
    if i == pressed {
        let d = pillH * 0.8
        NSColor(white: 1, alpha: 0.22).setFill()
        NSBezierPath(ovalIn: CGRect(x: c.x - d / 2, y: c.y - d / 2, width: d, height: d)).fill()
    }
    let cfg = NSImage.SymbolConfiguration(pointSize: icon, weight: .medium)
        .applying(NSImage.SymbolConfiguration(paletteColors: [NSColor(white: 1, alpha: i == pressed ? 1 : 0.86)]))
    guard let img = (NSImage(systemSymbolName: symbols[i], accessibilityDescription: nil)
                     ?? NSImage(systemSymbolName: fallback[i], accessibilityDescription: nil))?.withSymbolConfiguration(cfg) else { continue }
    let sz = img.size
    let k = min(icon * 1.3 / sz.width, icon * 1.15 / sz.height, 1.6)
    img.draw(in: CGRect(x: c.x - sz.width * k / 2, y: c.y - sz.height * k / 2, width: sz.width * k, height: sz.height * k))
}
NSGraphicsContext.restoreGraphicsState()
try? rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: a[6]))
