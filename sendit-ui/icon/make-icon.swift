// Draws sendit-ui's app icon: the "S" of the banner a VM shows when it
// starts (src/assets/banner.txt, ANSI Shadow letters in orange), on a tile
// that is light or dark as macOS is. The letter is drawn cell by cell as
// shapes, not with a font, so it stays crisp at every size.
//
//   swift make-icon.swift ICON_DIR
//
// writes, in ICON_DIR, for each of light and dark:
// - AppIcon.icon/Assets/background-*.png and letter-*.png: the layers of
//   AppIcon.icon, the Icon Composer document that actool makes the bundle's
//   icon of. So the bundle's tile is the same as the PNGs' below, the
//   document draws it from these and not from fills of its own;
// - sidebar-*.png: the tile alone at 72 px, for the sidebar, which shows
//   it at 36 points;
// - dock-*.png: the whole icon at 512 px, for the Dock when the app runs
//   outside its bundle.
// make-icon.sh runs it and actool. The glow is in the background layer, so
// that it goes with it when macOS hides that for a tinted icon.

import AppKit

/// The S, as the banner has it.
let letter = [
    "███████╗",
    "██╔════╝",
    "███████╗",
    "╚════██║",
    "███████║",
    "╚══════╝",
]

func srgb(_ r: CGFloat, _ g: CGFloat, _ b: CGFloat, _ a: CGFloat = 1) -> CGColor {
    CGColor(srgbRed: r, green: g, blue: b, alpha: a)
}

/// The colours of the icon in light or dark mode.
struct Variant {
    let name: String
    /// The tile's background, from top to bottom.
    let top: CGColor
    let bottom: CGColor
    let letter: CGColor
    /// How strongly the letter glows, as on a CRT.
    let glow: CGFloat
    /// The thin edge that sets the tile off from what's behind it.
    let edge: CGColor
}

let variants = [
    // A light terminal, with a deeper orange that stands out on it.
    Variant(
        name: "light", top: srgb(0.99, 0.99, 0.99), bottom: srgb(0.88, 0.88, 0.90),
        letter: srgb(0.94, 0.45, 0.0), glow: 0.10, edge: CGColor(gray: 0, alpha: 0.10)),
    // A dark terminal, with xterm colour 208, which the banner is printed in.
    Variant(
        name: "dark", top: srgb(0.20, 0.20, 0.22), bottom: srgb(0.08, 0.08, 0.09),
        letter: srgb(1.0, 0x87 / 255.0, 0.0), glow: 0.22, edge: CGColor(gray: 1, alpha: 0.12)),
]

/// Apple's icon grid: an 824-unit squircle in the middle of 1024.
let tile = CGRect(x: 100, y: 100, width: 824, height: 824)

/// Draws the tile on a 1024-unit canvas, y pointing down. With `shadow`,
/// the number of pixels per unit, it has the shadow macOS icons have under
/// them.
func drawTile(_ ctx: CGContext, _ v: Variant, shadow: CGFloat?) {
    let shape = CGPath(roundedRect: tile, cornerWidth: 185, cornerHeight: 185, transform: nil)

    ctx.saveGState()
    if let scale = shadow {
        // A shadow's offset and blur are in pixels, with y pointing up,
        // whatever the transform.
        ctx.setShadow(
            offset: CGSize(width: 0, height: -12 * scale), blur: 28 * scale,
            color: CGColor(gray: 0, alpha: 0.35))
    }
    ctx.addPath(shape)
    ctx.setFillColor(v.bottom)
    ctx.fillPath()
    ctx.restoreGState()

    ctx.saveGState()
    ctx.addPath(shape)
    ctx.clip()
    drawBackground(ctx, v)
    ctx.restoreGState()

    ctx.saveGState()
    ctx.addPath(
        CGPath(
            roundedRect: tile.insetBy(dx: 1.5, dy: 1.5), cornerWidth: 183.5,
            cornerHeight: 183.5, transform: nil))
    ctx.setStrokeColor(v.edge)
    ctx.setLineWidth(3)
    ctx.strokePath()
    ctx.restoreGState()
}

/// Draws the tile's background over all of the tile's square: a gradient,
/// and the letter's glow.
func drawBackground(_ ctx: CGContext, _ v: Variant) {
    let gradient = CGGradient(
        colorsSpace: CGColorSpace(name: CGColorSpace.sRGB),
        colors: [v.top, v.bottom] as CFArray, locations: [0, 1])!
    ctx.drawLinearGradient(
        gradient, start: CGPoint(x: 0, y: tile.minY), end: CGPoint(x: 0, y: tile.maxY),
        options: [.drawsBeforeStartLocation, .drawsAfterEndLocation])
    ctx.saveGState()
    ctx.addRect(tile)
    ctx.clip()
    let glow = CGGradient(
        colorsSpace: CGColorSpace(name: CGColorSpace.sRGB),
        colors: [v.letter.copy(alpha: v.glow)!, v.letter.copy(alpha: 0)!] as CFArray,
        locations: [0, 1])!
    ctx.drawRadialGradient(
        glow, startCenter: CGPoint(x: 512, y: 512), startRadius: 0,
        endCenter: CGPoint(x: 512, y: 512), endRadius: 420, options: [])
    ctx.restoreGState()
}

/// Draws `letter` in the middle of the tile, with terminal-shaped cells.
func drawLetter(_ ctx: CGContext, _ color: CGColor) {
    let cols = CGFloat(letter[0].count)
    let rows = CGFloat(letter.count)
    let cellW: CGFloat = 56
    let cellH: CGFloat = 92
    let origin = CGPoint(x: 512 - cols * cellW / 2, y: 512 - rows * cellH / 2)

    // The double lines: their distance from the cell's middle, and width.
    let d = cellW * 0.2
    let t = cellW * 0.13

    ctx.setFillColor(color)
    ctx.setStrokeColor(color)
    ctx.setLineWidth(t)
    ctx.setLineCap(.butt)
    ctx.setLineJoin(.miter)

    // The blocks, filled as one shape, and the lines, stroked as one:
    // drawn cell by cell, anti-aliasing leaves seams between the cells at
    // small sizes.
    let blocks = CGMutablePath()
    let lines = CGMutablePath()
    for (row, line) in letter.enumerated() {
        for (col, ch) in line.enumerated() {
            let x0 = origin.x + CGFloat(col) * cellW
            let y0 = origin.y + CGFloat(row) * cellH
            let x1 = x0 + cellW
            let y1 = y0 + cellH
            let cx = x0 + cellW / 2
            let cy = y0 + cellH / 2

            // Each path is a list of points, a line through them.
            var paths: [[CGPoint]] = []
            switch ch {
            case "█":
                blocks.addRect(CGRect(x: x0, y: y0, width: cellW, height: cellH))
            case "═":
                paths = [
                    [CGPoint(x: x0, y: cy - d), CGPoint(x: x1, y: cy - d)],
                    [CGPoint(x: x0, y: cy + d), CGPoint(x: x1, y: cy + d)],
                ]
            case "║":
                paths = [
                    [CGPoint(x: cx - d, y: y0), CGPoint(x: cx - d, y: y1)],
                    [CGPoint(x: cx + d, y: y0), CGPoint(x: cx + d, y: y1)],
                ]
            case "╗":  // from the left, down
                paths = [
                    [CGPoint(x: x0, y: cy - d), CGPoint(x: cx + d, y: cy - d), CGPoint(x: cx + d, y: y1)],
                    [CGPoint(x: x0, y: cy + d), CGPoint(x: cx - d, y: cy + d), CGPoint(x: cx - d, y: y1)],
                ]
            case "╔":  // to the right, down
                paths = [
                    [CGPoint(x: x1, y: cy - d), CGPoint(x: cx - d, y: cy - d), CGPoint(x: cx - d, y: y1)],
                    [CGPoint(x: x1, y: cy + d), CGPoint(x: cx + d, y: cy + d), CGPoint(x: cx + d, y: y1)],
                ]
            case "╝":  // from the left, up
                paths = [
                    [CGPoint(x: x0, y: cy + d), CGPoint(x: cx + d, y: cy + d), CGPoint(x: cx + d, y: y0)],
                    [CGPoint(x: x0, y: cy - d), CGPoint(x: cx - d, y: cy - d), CGPoint(x: cx - d, y: y0)],
                ]
            case "╚":  // to the right, up
                paths = [
                    [CGPoint(x: x1, y: cy + d), CGPoint(x: cx - d, y: cy + d), CGPoint(x: cx - d, y: y0)],
                    [CGPoint(x: x1, y: cy - d), CGPoint(x: cx + d, y: cy - d), CGPoint(x: cx + d, y: y0)],
                ]
            case " ":
                break
            default:
                fatalError("no shape for \(ch)")
            }
            for points in paths {
                lines.addLines(between: points)
            }
        }
    }
    ctx.addPath(lines)
    ctx.strokePath()
    ctx.addPath(blocks)
    ctx.fillPath()
}

/// What of the icon to render.
enum Part {
    /// All of it, with the margin and shadow around the tile.
    case whole
    /// The tile alone, filling the image.
    case tile
    /// The letter alone, where the tile would fill the image.
    case letter
    /// The tile's background alone, filling the image.
    case background
}

/// Renders `part` of the icon at `pixels` square to a PNG at `url`.
func render(_ part: Part, _ v: Variant, pixels: Int, to url: URL) throws {
    let ctx = CGContext(
        data: nil, width: pixels, height: pixels, bitsPerComponent: 8, bytesPerRow: 0,
        space: CGColorSpace(name: CGColorSpace.sRGB)!,
        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    let scale = CGFloat(pixels) / (part == .whole ? 1024 : tile.width)
    // y down, in 1024 units.
    ctx.translateBy(x: 0, y: CGFloat(pixels))
    ctx.scaleBy(x: scale, y: -scale)
    if part != .whole {
        ctx.translateBy(x: -tile.minX, y: -tile.minY)
    }
    switch part {
    case .whole:
        drawTile(ctx, v, shadow: scale)
        drawLetter(ctx, v.letter)
    case .tile:
        drawTile(ctx, v, shadow: nil)
        drawLetter(ctx, v.letter)
    case .letter:
        drawLetter(ctx, v.letter)
    case .background:
        drawBackground(ctx, v)
    }
    let rep = NSBitmapImageRep(cgImage: ctx.makeImage()!)
    try rep.representation(using: .png, properties: [:])!.write(to: url)
}

guard CommandLine.arguments.count == 2 else {
    FileHandle.standardError.write(Data("usage: swift make-icon.swift ICON_DIR\n".utf8))
    exit(2)
}
let dir = URL(fileURLWithPath: CommandLine.arguments[1])
let assets = dir.appendingPathComponent("AppIcon.icon/Assets")
do {
    try FileManager.default.createDirectory(at: assets, withIntermediateDirectories: true)
    for v in variants {
        try render(
            .letter, v, pixels: 1024, to: assets.appendingPathComponent("letter-\(v.name).png"))
        try render(
            .background, v, pixels: 1024,
            to: assets.appendingPathComponent("background-\(v.name).png"))
        try render(.tile, v, pixels: 72, to: dir.appendingPathComponent("sidebar-\(v.name).png"))
        try render(.whole, v, pixels: 512, to: dir.appendingPathComponent("dock-\(v.name).png"))
    }
} catch {
    FileHandle.standardError.write(Data("make-icon.swift: \(error.localizedDescription)\n".utf8))
    exit(1)
}
