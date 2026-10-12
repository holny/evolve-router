// alpha.swift — 黑白底 PNG → 黑色+alpha 模板图标（alpha = 255 − 亮度）
// 用法: swift alpha.swift <input.png> <output.png>
import Foundation
import CoreGraphics
import ImageIO

let args = CommandLine.arguments
guard args.count == 3 else { fatalError("usage: alpha.swift <in.png> <out.png>") }
let inURL = URL(fileURLWithPath: args[1]) as CFURL
let outURL = URL(fileURLWithPath: args[2]) as CFURL

guard let src = CGImageSourceCreateWithURL(inURL, nil),
      let img = CGImageSourceCreateImageAtIndex(src, 0, nil) else { fatalError("cannot read \(args[1])") }
let w = img.width, h = img.height
let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w * 4,
                    space: CGColorSpaceCreateDeviceRGB(),
                    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
ctx.draw(img, in: CGRect(x: 0, y: 0, width: w, height: h))
let buf = ctx.data!.bindMemory(to: UInt8.self, capacity: w * h * 4)
for i in stride(from: 0, to: w * h * 4, by: 4) {
    let lum = (299 * Int(buf[i]) + 587 * Int(buf[i + 1]) + 114 * Int(buf[i + 2])) / 1000
    buf[i] = 0; buf[i + 1] = 0; buf[i + 2] = 0
    buf[i + 3] = UInt8(max(0, min(255, 255 - lum)))
}
let out = ctx.makeImage()!
guard let dest = CGImageDestinationCreateWithURL(outURL, "public.png" as CFString, 1, nil) else { fatalError("cannot create \(args[2])") }
CGImageDestinationAddImage(dest, out, nil)
guard CGImageDestinationFinalize(dest) else { fatalError("write fail") }
print("alpha 提取完成: \(args[2]) (\(w)x\(h))")
