// Draws legcon's app icon: a beige 1990s monitor with a green prompt, on a blue tile.
// Run by bundle/make-icon.sh: `swift make-icon.swift out.png` (1024 x 1024).
import AppKit

let size = 1024.0
let out = CommandLine.arguments[1]
let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(size), pixelsHigh: Int(size),
                           bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                           colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)

func color(_ hex: UInt32) -> NSColor {
    NSColor(srgbRed: CGFloat((hex >> 16) & 0xff) / 255, green: CGFloat((hex >> 8) & 0xff) / 255,
            blue: CGFloat(hex & 0xff) / 255, alpha: 1)
}

// The tile, inset as macOS icons are.
let tile = NSBezierPath(roundedRect: NSRect(x: 100, y: 100, width: 824, height: 824), xRadius: 185, yRadius: 185)
NSGradient(starting: color(0x2f6fb5), ending: color(0x16365f))!.draw(in: tile, angle: -90)

// The monitor: case, screen, stand.
color(0x8a8270).setFill()
NSBezierPath(roundedRect: NSRect(x: 400, y: 215, width: 224, height: 70), xRadius: 14, yRadius: 14).fill()
color(0xd9d2bd).setFill()
NSBezierPath(roundedRect: NSRect(x: 340, y: 255, width: 344, height: 50), xRadius: 16, yRadius: 16).fill()
NSBezierPath(roundedRect: NSRect(x: 215, y: 290, width: 594, height: 500), xRadius: 48, yRadius: 48).fill()
color(0xb8af96).setStroke()
let bezel = NSBezierPath(roundedRect: NSRect(x: 215, y: 290, width: 594, height: 500), xRadius: 48, yRadius: 48)
bezel.lineWidth = 10
bezel.stroke()
color(0x10140f).setFill()
NSBezierPath(roundedRect: NSRect(x: 265, y: 350, width: 494, height: 390), xRadius: 30, yRadius: 30).fill()

// A prompt on the screen.
let prompt = NSAttributedString(string: "C:\\> lc_", attributes: [
    .font: NSFont(name: "Menlo-Bold", size: 78) ?? NSFont.monospacedSystemFont(ofSize: 78, weight: .bold),
    .foregroundColor: color(0x5dff7a),
])
prompt.draw(at: NSPoint(x: 300, y: 560))
let line = NSAttributedString(string: "ftp telnet smb", attributes: [
    .font: NSFont(name: "Menlo", size: 50) ?? NSFont.monospacedSystemFont(ofSize: 50, weight: .regular),
    .foregroundColor: color(0x2fa043),
])
line.draw(at: NSPoint(x: 300, y: 440))

// The power light.
color(0x2fcf4f).setFill()
NSBezierPath(ovalIn: NSRect(x: 740, y: 308, width: 22, height: 22)).fill()

NSGraphicsContext.restoreGraphicsState()
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: out))
