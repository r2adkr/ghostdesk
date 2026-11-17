// Draws the app icon in every form the builds need and the .dmg window background:
//   packaging/macos/AppIcon.icns          macOS bundle icon
//   packaging/macos/dmg-background.tiff   .dmg window background (1x + 2x)
//   assets/app-icon.png                   window icon set at runtime (Windows taskbar, macOS Dock)
//   windows/ghostdesk.res                 Windows exe icon resource (linked by build.rs)
//
//   swift packaging/macos/make-art.swift
//
// Run from the repository root after changing the art; the outputs are committed next to this
// file. The icon is the ghost from the app's own icon font (assets/lucide/lucide.ttf, the same
// glyph as in the title bar) in the app's off-white on a graphite tile, so it reads as GhostDesk
// in the Dock and the dmg without any decoration.

import AppKit
import CoreText

let root = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
let outDir = root.appendingPathComponent("packaging/macos")
let work = URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent("ghostdesk-art-\(getpid())")
try? FileManager.default.removeItem(at: work)
try! FileManager.default.createDirectory(at: work, withIntermediateDirectories: true)

// app colors (src/app/theme.rs, dark palette)
func rgb(_ hex: Int, _ a: CGFloat = 1) -> NSColor {
    NSColor(srgbRed: CGFloat((hex >> 16) & 0xff) / 255, green: CGFloat((hex >> 8) & 0xff) / 255, blue: CGFloat(hex & 0xff) / 255, alpha: a)
}

// the ghost glyph (U+E20E in lucide.ttf)
let fontURL = root.appendingPathComponent("assets/lucide/lucide.ttf") as CFURL
guard let descs = CTFontManagerCreateFontDescriptorsFromURL(fontURL) as? [CTFontDescriptor], let desc = descs.first else {
    fatalError("lucide.ttf not found; run from the repository root")
}

func ghostPath(size: CGFloat) -> CGPath {
    let font = CTFontCreateWithFontDescriptor(desc, size, nil)
    var ch: UniChar = 0xE20E
    var glyph: CGGlyph = 0
    CTFontGetGlyphsForCharacters(font, &ch, &glyph, 1)
    return CTFontCreatePathForGlyph(font, glyph, nil)!
}

func png(_ size: Int, _ draw: (CGContext, CGFloat) -> Void) -> Data {
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: size, pixelsHigh: size, bitsPerSample: 8, samplesPerPixel: 4,
                               hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    draw(NSGraphicsContext.current!.cgContext, CGFloat(size))
    NSGraphicsContext.restoreGraphicsState()
    return rep.representation(using: .png, properties: [:])!
}

// ---- app icon ----

/// macOS icon grid: the tile is 824/1024 of the canvas with a ~185/1024 corner, centered, with a
/// soft shadow below.
func drawIcon(_ c: CGContext, _ s: CGFloat) {
    let k = s / 1024
    let tile = CGRect(x: 100 * k, y: 100 * k, width: 824 * k, height: 824 * k)
    let shape = CGPath(roundedRect: tile, cornerWidth: 185 * k, cornerHeight: 185 * k, transform: nil)
    c.saveGState()
    c.setShadow(offset: CGSize(width: 0, height: -10 * k), blur: 28 * k, color: NSColor(white: 0, alpha: 0.35).cgColor)
    c.addPath(shape)
    c.setFillColor(rgb(0x1b1b1b).cgColor)
    c.fillPath()
    c.restoreGState()
    // a small tone shift inside the graphite (lighter at the top), no hue change
    c.saveGState()
    c.addPath(shape)
    c.clip()
    let grad = CGGradient(colorsSpace: CGColorSpaceCreateDeviceRGB(), colors: [rgb(0x2c2c2c).cgColor, rgb(0x181818).cgColor] as CFArray, locations: [0, 1])!
    c.drawLinearGradient(grad, start: CGPoint(x: 0, y: tile.maxY), end: CGPoint(x: 0, y: tile.minY), options: [])
    c.restoreGState()
    // hairline edge
    c.addPath(CGPath(roundedRect: tile.insetBy(dx: 1.5 * k, dy: 1.5 * k), cornerWidth: 183 * k, cornerHeight: 183 * k, transform: nil))
    c.setStrokeColor(NSColor(white: 1, alpha: 0.08).cgColor)
    c.setLineWidth(3 * k)
    c.strokePath()
    // the ghost, centered optically (a touch above the middle)
    let glyph = ghostPath(size: 520 * k)
    let b = glyph.boundingBoxOfPath
    var t = CGAffineTransform(translationX: tile.midX - b.midX, y: tile.midY - b.midY + 12 * k)
    let placed = glyph.copy(using: &t)!
    c.addPath(placed)
    c.setFillColor(rgb(0xf1f1f1).cgColor)
    c.fillPath()
}

let iconset = work.appendingPathComponent("AppIcon.iconset")
try! FileManager.default.createDirectory(at: iconset, withIntermediateDirectories: true)
for base in [16, 32, 128, 256, 512] {
    try! png(base, drawIcon).write(to: iconset.appendingPathComponent("icon_\(base)x\(base).png"))
    try! png(base * 2, drawIcon).write(to: iconset.appendingPathComponent("icon_\(base)x\(base)@2x.png"))
}
let iconutil = Process()
iconutil.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
iconutil.arguments = ["-c", "icns", iconset.path, "-o", outDir.appendingPathComponent("AppIcon.icns").path]
try! iconutil.run()
iconutil.waitUntilExit()
precondition(iconutil.terminationStatus == 0, "iconutil failed")

// ---- dmg background ----

// Window content 640x400 points. Icons (dmg_settings.py) sit at x 180 and 460, y 160, 112pt.
let W: CGFloat = 640, H: CGFloat = 400

func drawBackground(scale: CGFloat) -> Data {
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(W * scale), pixelsHigh: Int(H * scale), bitsPerSample: 8,
                               samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    rep.size = NSSize(width: W, height: H)
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    let c = NSGraphicsContext.current!.cgContext
    // light neutral, so Finder's dark icon labels stay readable
    c.setFillColor(rgb(0xf5f5f4).cgColor)
    c.fill(CGRect(x: 0, y: 0, width: W, height: H))
    // arrow from the app to Applications (Finder's y goes down; here y goes up)
    let y = H - 160
    let arrow = CGMutablePath()
    arrow.move(to: CGPoint(x: 268, y: y))
    arrow.addLine(to: CGPoint(x: 372, y: y))
    arrow.move(to: CGPoint(x: 360, y: y + 10))
    arrow.addLine(to: CGPoint(x: 373, y: y))
    arrow.addLine(to: CGPoint(x: 360, y: y - 10))
    c.addPath(arrow)
    c.setStrokeColor(rgb(0xa3a3a3).cgColor)
    c.setLineWidth(2)
    c.setLineCap(.round)
    c.setLineJoin(.round)
    c.strokePath()
    // two quiet lines of instructions at the bottom
    let para = NSMutableParagraphStyle()
    para.alignment = .center
    func line(_ s: String, _ size: CGFloat, _ color: NSColor, _ top: CGFloat, weight: NSFont.Weight = .regular) {
        let attrs: [NSAttributedString.Key: Any] = [.font: NSFont.systemFont(ofSize: size, weight: weight), .foregroundColor: color, .paragraphStyle: para]
        NSAttributedString(string: s, attributes: attrs).draw(in: CGRect(x: 0, y: H - top - size * 1.4, width: W, height: size * 1.6))
    }
    line("GhostDesk 를 응용 프로그램 폴더로 끌어 놓으세요", 13, rgb(0x3a3a3a), 280, weight: .medium)
    line("Drag GhostDesk to Applications. Opening it from here installs it too.", 11, rgb(0x8a8a8a), 304)
    NSGraphicsContext.restoreGraphicsState()
    return rep.representation(using: .png, properties: [:])!
}

let bg1 = work.appendingPathComponent("bg.png"), bg2 = work.appendingPathComponent("bg@2x.png")
try! drawBackground(scale: 1).write(to: bg1)
try! drawBackground(scale: 2).write(to: bg2)
let tiff = Process()
tiff.executableURL = URL(fileURLWithPath: "/usr/bin/tiffutil")
tiff.arguments = ["-cathidpicheck", bg1.path, bg2.path, "-out", outDir.appendingPathComponent("dmg-background.tiff").path]
try! tiff.run()
tiff.waitUntilExit()
precondition(tiff.terminationStatus == 0, "tiffutil failed")
// ---- window icon and Windows exe icon ----

// window/taskbar icon set at runtime (otherwise eframe shows its own default icon)
try! png(256, drawIcon).write(to: root.appendingPathComponent("assets/app-icon.png"))

/// Windows resource file with the exe icon (RT_ICON entries as PNG, plus the RT_GROUP_ICON that
/// Explorer uses), linked into ghostdesk.exe by build.rs. Written by hand so the build needs no
/// resource compiler.
func le16(_ v: Int) -> [UInt8] { [UInt8(v & 0xff), UInt8((v >> 8) & 0xff)] }
func le32(_ v: Int) -> [UInt8] { le16(v & 0xffff) + le16((v >> 16) & 0xffff) }
func resEntry(type: Int, name: Int, flags: Int, data: [UInt8]) -> [UInt8] {
    var h: [UInt8] = []
    h += le32(data.count) + le32(32)
    h += le16(0xffff) + le16(type) + le16(0xffff) + le16(name)
    h += le32(0) + le16(flags) + le16(0) + le32(0) + le32(0)
    var out = h + data
    while out.count % 4 != 0 { out.append(0) }
    return out
}
var res: [UInt8] = resEntry(type: 0, name: 0, flags: 0, data: [])
let icoSizes = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]
var group: [UInt8] = le16(0) + le16(1) + le16(icoSizes.count)
for (i, px) in icoSizes.enumerated() {
    let data = [UInt8](png(px, drawIcon))
    res += resEntry(type: 3, name: i + 1, flags: 0x1010, data: data)
    let dim = UInt8(px >= 256 ? 0 : px)
    group += [dim, dim, 0, 0] + le16(1) + le16(32) + le32(data.count) + le16(i + 1)
}
res += resEntry(type: 14, name: 1, flags: 0x1030, data: group)
try! Data(res).write(to: root.appendingPathComponent("windows/ghostdesk.res"))

// previews for review
try? FileManager.default.copyItem(at: bg2, to: work.appendingPathComponent("preview-bg.png"))
try! png(512, drawIcon).write(to: work.appendingPathComponent("preview-icon.png"))
print(work.path)
