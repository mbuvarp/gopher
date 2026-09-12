// Render the Dev channel badge as native vector UI over the unchanged app artwork.
import AppKit

let arguments = CommandLine.arguments
guard arguments.count == 3, let original = NSImage(contentsOfFile: arguments[1]) else {
    fatalError("Usage: dev_icon.swift SOURCE.png DESTINATION.png")
}
let size = NSSize(width: 1024, height: 1024)
let image = NSImage(size: size)
image.lockFocus()
original.draw(in: NSRect(origin: .zero, size: size))
let badge = NSRect(x: 650, y: 110, width: 270, height: 150)
NSColor.white.setFill()
NSBezierPath(roundedRect: badge.insetBy(dx: -10, dy: -10), xRadius: 55, yRadius: 55).fill()
NSColor(calibratedRed: 0.0, green: 0.38, blue: 1.0, alpha: 1.0).setFill()
NSBezierPath(roundedRect: badge, xRadius: 45, yRadius: 45).fill()
let text = "DEV" as NSString
let attributes: [NSAttributedString.Key: Any] = [
    .font: NSFont.boldSystemFont(ofSize: 92),
    .foregroundColor: NSColor.white,
]
let textSize = text.size(withAttributes: attributes)
text.draw(at: NSPoint(x: badge.midX - textSize.width / 2, y: badge.midY - textSize.height / 2), withAttributes: attributes)
image.unlockFocus()
guard let tiff = image.tiffRepresentation,
      let bitmap = NSBitmapImageRep(data: tiff),
      let png = bitmap.representation(using: .png, properties: [:]) else {
    fatalError("Could not render the Dev app icon")
}
try png.write(to: URL(fileURLWithPath: arguments[2]))
