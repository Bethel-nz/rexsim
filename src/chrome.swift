// Draws rexsim's scene around the live screen: the phone's own wallpaper behind it and a
// real device frame over it (from the Apple Frames pack that frames-cli uses).
//   chrome <W> <H> <sx> <sy> <sw> <sh> <bg.png> <frame.png>
// W×H is the pane in pixels; (sx, sy, sw, sh) is where the live screen sits in it, from
// the top left. Prints one line per side button the frame has: "btn <name> x y w h".
import AppKit
import ImageIO

let a = CommandLine.arguments
guard a.count == 9, let W = Double(a[1]), let H = Double(a[2]), let sx = Double(a[3]), let sy = Double(a[4]),
      let sw = Double(a[5]), let sh = Double(a[6]) else { exit(1) }
let bgOut = a[7], frameOut = a[8]
let home = NSHomeDirectory()
let store = home + "/Library/Application Support/rexsim"

func run(_ tool: String, _ args: [String]) -> String {
    let p = Process(); p.executableURL = URL(fileURLWithPath: tool); p.arguments = args
    let pipe = Pipe(); p.standardOutput = pipe; p.standardError = FileHandle.nullDevice
    guard (try? p.run()) != nil else { return "" }
    let d = pipe.fileHandleForReading.readDataToEndOfFile(); p.waitUntilExit()
    return String(decoding: d, as: UTF8.self)
}

func load(_ path: String) -> CGImage? {
    guard let src = CGImageSourceCreateWithURL(URL(fileURLWithPath: path) as CFURL, nil) else { return nil }
    return CGImageSourceCreateImageAtIndex(src, 0, nil)
}

func canvas() -> CGContext {
    CGContext(data: nil, width: Int(W), height: Int(H), bitsPerComponent: 8, bytesPerRow: 0,
              space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
}

func save(_ ctx: CGContext, _ path: String) {
    guard let img = ctx.makeImage(), let dest = CGImageDestinationCreateWithURL(URL(fileURLWithPath: path) as CFURL, "public.png" as CFString, 1, nil) else { return }
    CGImageDestinationAddImage(dest, img, nil); CGImageDestinationFinalize(dest)
}

// The booted device: its folder and its model name ("iPhone 17 Pro").
var udid = "", model = ""
if let j = try? JSONSerialization.jsonObject(with: Data(run("/usr/bin/xcrun", ["simctl", "list", "devices", "booted", "-j"]).utf8)) as? [String: Any],
   let devs = j["devices"] as? [String: [[String: Any]]], let d = devs.values.flatMap({ $0 }).first {
    udid = d["udid"] as? String ?? ""
    let type = d["deviceTypeIdentifier"] as? String ?? ""
    model = type.components(separatedBy: ".").last?.replacingOccurrences(of: "-", with: " ") ?? ""
}

if let m = ProcessInfo.processInfo.environment["REXSIM_MODEL"] { model = m }   // for testing without a device

// MARK: Wallpaper — the snapshot iOS keeps of the home screen wallpaper, light or dark.

func wallpaper() -> CGImage? {
    guard !udid.isEmpty else { return nil }
    let style = run("/usr/bin/xcrun", ["simctl", "ui", udid, "appearance"]).contains("dark") ? 2 : 1
    let root = home + "/Library/Developer/CoreSimulator/Devices/\(udid)/data/Library/Application Support/PRBPosterExtensionDataStore"
    guard let e = FileManager.default.enumerator(atPath: root) else { return nil }
    var best: (String, Date)?
    for case let rel as String in e where rel.hasSuffix("Resources/Composite.atx") {
        let full = root + "/" + rel
        let info = (full as NSString).deletingLastPathComponent + "/../Contents/Info.plist"
        guard let pl = NSDictionary(contentsOfFile: info),
              (pl["PUIPosterSnapshotBundleInfoKeySnapshotDefinitionIdentifier"] as? String)?.contains("UNLOCKED") == true,
              (pl["PUIPosterSnapshotBundleInfoKeyUserInterfaceStyle"] as? Int ?? 1) == style,
              let date = try? FileManager.default.attributesOfItem(atPath: full)[.modificationDate] as? Date else { continue }
        if best == nil || date > best!.1 { best = (full, date) }
    }
    return best.flatMap { load($0.0) }
}

let bg = canvas()
if let wp = wallpaper() {
    let s = max(W / Double(wp.width), H / Double(wp.height))
    let w = Double(wp.width) * s, h = Double(wp.height) * s
    bg.interpolationQuality = .high
    bg.draw(wp, in: CGRect(x: (W - w) / 2, y: (H - h) / 2, width: w, height: h))
    bg.setFillColor(CGColor(gray: 0, alpha: 0.12)); bg.fill(CGRect(x: 0, y: 0, width: W, height: H))
}
save(bg, bgOut)

// MARK: Frame — from the Apple Frames pack, downloaded once like frames-cli does.

let packURL = "https://cdn.macstories.net/images/uploads/2026/11/10/appleframes421-1794327573824-2869ef1354.zip"
let frames = store + "/frames"

func ensurePack() {
    if FileManager.default.fileExists(atPath: frames + "/NewFrames.json") { return }
    try? FileManager.default.createDirectory(atPath: frames, withIntermediateDirectories: true)
    let zip = NSTemporaryDirectory() + "rexsim-frames.zip"
    _ = run("/usr/bin/curl", ["-fsSL", "-o", zip, packURL])
    // Only the parts rexsim uses: the index and the portrait iPhone frames and masks.
    _ = run("/usr/bin/unzip", ["-q", "-o", "-j", zip, "Frames/NewFrames.json", "Frames/iPhone*Portrait.png", "Frames/iPhone*Portrait_mask.png", "-d", frames])
    try? FileManager.default.removeItem(atPath: zip)
}

/// The frame picture, and where the screen sits in it (x, y, w, h in its pixels).
func frame(for model: String) -> (CGImage, CGRect)? {
    ensurePack()
    let name = "\(model) Portrait"
    guard let img = load(frames + "/\(name).png"), let mask = load(frames + "/\(name)_mask.png"),
          let data = FileManager.default.contents(atPath: frames + "/NewFrames.json"),
          let json = try? JSONSerialization.jsonObject(with: data) else { return nil }
    var offset: CGPoint?
    func walk(_ o: Any) {
        if let d = o as? [String: Any] {
            if d["name"] as? String == name, let x = Double("\(d["x"] ?? "")"), let y = Double("\(d["y"] ?? "")") { offset = CGPoint(x: x, y: y) }
            d.values.forEach(walk)
        } else if let l = o as? [Any] { l.forEach(walk) }
    }
    walk(json)
    guard let o = offset else { return nil }
    return (img, CGRect(x: o.x, y: o.y, width: Double(mask.width), height: Double(mask.height)))
}

let fr = canvas()
if let (img, scr) = frame(for: model) {
    // Map the frame's screen opening onto the live screen, a hair smaller so the bezel
    // covers the screen's edge and no gap can show.
    let inset = 2.0
    let kx = (sw - inset * 2) / scr.width, ky = (sh - inset * 2) / scr.height
    let ox = sx + inset - scr.minX * kx, oy = sy + inset - scr.minY * ky   // top-left origin
    let rect = CGRect(x: ox, y: H - (oy + Double(img.height) * ky), width: Double(img.width) * kx, height: Double(img.height) * ky)
    fr.interpolationQuality = .high
    fr.draw(img, in: rect)
    save(fr, frameOut)

    // Side buttons: the parts of the frame that stick out past the body on each side.
    // Read alpha from an RGBA copy (the pack's PNGs can be palette images).
    let iw = img.width, ih = img.height
    var buf = [UInt8](repeating: 0, count: iw * ih * 4)
    guard let rgba = CGContext(data: &buf, width: iw, height: ih, bitsPerComponent: 8, bytesPerRow: iw * 4,
                               space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { exit(0) }
    rgba.draw(img, in: CGRect(x: 0, y: 0, width: iw, height: ih))
    // The buffer's first row is the top of the picture, like the pack's numbers.
    func alpha(_ x: Int, _ y: Int) -> UInt8 { buf[(y * iw + x) * 4 + 3] }
    func spans(_ x: Int) -> [(Int, Int)] {
        var out: [(Int, Int)] = [], start: Int?
        for y in 0..<img.height {
            let on = alpha(x, y) > 100
            if on, start == nil { start = y }
            if !on, let s = start { if y - s > 20 { out.append((s, y)) }; start = nil }
        }
        return out
    }
    let left = spans(3), right = spans(img.width - 4)
    // Left, top to bottom: Action (shortest, first), then volume up and down.
    let leftNames = left.count == 3 ? ["action", "volup", "voldown"] : left.count == 2 ? ["volup", "voldown"] : []
    func emit(_ name: String, _ x0: Double, _ x1: Double, _ s: (Int, Int)) {
        let x = ox + x0 * kx, y = oy + Double(s.0) * ky
        print("btn \(name) \(Int(x)) \(Int(y)) \(Int((x1 - x0) * kx)) \(Int(Double(s.1 - s.0) * ky))")
    }
    for (n, s) in zip(leftNames, left) { emit(n, 0, scr.minX, s) }
    if let s = right.first { emit("lock", scr.maxX, Double(img.width), s) }
} else {
    save(fr, frameOut)
}
