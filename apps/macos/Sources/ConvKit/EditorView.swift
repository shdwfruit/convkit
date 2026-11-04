import AVFoundation
import AppKit
import ImageIO
import Observation
import SwiftUI
import UniformTypeIdentifiers

// MARK: - The file, for the preview

/// The file being edited, loaded for the preview: its picture, its size as shown, and
/// a player for video and sound.
@Observable
final class MediaPreview {
    private(set) var url: URL?
    private(set) var image: NSImage?
    /// Pixels as the file is shown, with its EXIF orientation or video rotation applied.
    /// conv's --crop counts in these.
    private(set) var size: CGSize = .zero
    private(set) var player: AVPlayer?
    private(set) var duration: Double = 0
    private(set) var isGIF = false
    private(set) var current: Double = 0
    private(set) var playing = false
    /// Playback plays the part being kept, at the speed chosen.
    var range: ClosedRange<Double> = 0...0
    var rate: Float = 1
    private var observer: Any?

    @MainActor
    func load(_ url: URL?, media: Media) async {
        guard url != self.url else { return }
        stop()
        self.url = url
        image = nil
        size = .zero
        duration = 0
        current = 0
        player = nil
        isGIF = false
        guard let url else { return }
        if media == .image || fileExt(url) == "gif" {
            loadImage(url)
            if fileExt(url) == "gif" { loadGIFTiming(url) }
        } else {
            await loadAV(url)
        }
        range = 0...max(duration, 0)
    }

    private func loadImage(_ url: URL) {
        if let src = CGImageSourceCreateWithURL(url as CFURL, nil),
           let props = CGImageSourceCopyPropertiesAtIndex(src, 0, nil) as? [CFString: Any] {
            let w = props[kCGImagePropertyPixelWidth] as? Int ?? 0
            let h = props[kCGImagePropertyPixelHeight] as? Int ?? 0
            let o = props[kCGImagePropertyOrientation] as? Int ?? 1
            size = o >= 5 ? CGSize(width: h, height: w) : CGSize(width: w, height: h)
            let opts: [CFString: Any] = [
                kCGImageSourceCreateThumbnailFromImageAlways: true,
                kCGImageSourceCreateThumbnailWithTransform: true,
                kCGImageSourceThumbnailMaxPixelSize: 2400,
            ]
            if let cg = CGImageSourceCreateThumbnailAtIndex(src, 0, opts as CFDictionary) {
                image = NSImage(cgImage: cg, size: NSSize(width: cg.width, height: cg.height))
            }
        }
        // SVG and the like: ImageIO can't read them, NSImage can.
        if image == nil, let img = NSImage(contentsOf: url) {
            image = img
            if size == .zero { size = img.size }
        }
    }

    private func loadGIFTiming(_ url: URL) {
        isGIF = true
        guard let src = CGImageSourceCreateWithURL(url as CFURL, nil) else { return }
        var total = 0.0
        for i in 0..<CGImageSourceGetCount(src) {
            let p = CGImageSourceCopyPropertiesAtIndex(src, i, nil) as? [CFString: Any]
            let g = p?[kCGImagePropertyGIFDictionary] as? [CFString: Any]
            total += (g?[kCGImagePropertyGIFUnclampedDelayTime] as? Double)
                ?? (g?[kCGImagePropertyGIFDelayTime] as? Double) ?? 0.1
        }
        duration = total
    }

    @MainActor
    private func loadAV(_ url: URL) async {
        let asset = AVURLAsset(url: url)
        let p = AVPlayer(playerItem: AVPlayerItem(asset: asset))
        player = p
        duration = (try? await asset.load(.duration)).map(CMTimeGetSeconds) ?? 0
        if let track = try? await asset.loadTracks(withMediaType: .video).first,
           let loaded = try? await track.load(.naturalSize, .preferredTransform) {
            let r = CGRect(origin: .zero, size: loaded.0).applying(loaded.1)
            size = CGSize(width: abs(r.width), height: abs(r.height))
        }
        observer = p.addPeriodicTimeObserver(forInterval: CMTime(value: 1, timescale: 30), queue: .main) { [weak self] t in
            self?.tick(CMTimeGetSeconds(t))
        }
    }

    private func tick(_ t: Double) {
        current = t
        if playing && t >= range.upperBound - 0.01 { pause() }
    }

    func togglePlay() { playing ? pause() : play() }

    func play() {
        guard let player else { return }
        if current < range.lowerBound || current >= range.upperBound - 0.05 { seek(range.lowerBound) }
        player.rate = rate
        playing = true
    }

    func pause() {
        player?.pause()
        playing = false
    }

    func seek(_ t: Double) {
        current = t
        player?.seek(to: CMTime(seconds: t, preferredTimescale: 600), toleranceBefore: .zero, toleranceAfter: .zero)
    }

    func stop() {
        pause()
        if let observer { player?.removeTimeObserver(observer) }
        observer = nil
    }
}

// MARK: - Geometry

/// conv's --rotate and --flip as quarter turns clockwise and mirrors. conv turns first,
/// then flips, and the preview does the same.
struct Turn {
    var quarters: Int
    var flipH: Bool
    var flipV: Bool

    init(rotate: String, flip: String) {
        quarters = ["right": 1, "180": 2, "left": 3][rotate] ?? 0
        flipH = flip == "h"
        flipV = flip == "v"
    }

    var rotate: String { ["", "right", "180", "left"][((quarters % 4) + 4) % 4] }
    var flip: String { flipH ? "h" : flipV ? "v" : "" }
    var sideways: Bool { quarters % 2 != 0 }

    /// Mirroring both ways is half a turn, and conv takes one flip at a time.
    mutating func normalize() {
        if flipH && flipV {
            flipH = false
            flipV = false
            quarters += 2
        }
        quarters = ((quarters % 4) + 4) % 4
    }

    func shown(_ size: CGSize) -> CGSize { sideways ? CGSize(width: size.height, height: size.width) : size }

    var summary: String {
        var parts: [String] = []
        switch quarters % 4 {
        case 1: parts.append("Turned right")
        case 2: parts.append("Upside down")
        case 3: parts.append("Turned left")
        default: break
        }
        if flipH { parts.append("mirrored") }
        if flipV { parts.append("flipped top to bottom") }
        return parts.isEmpty ? "As it is" : parts.joined(separator: ", ")
    }
}

/// conv's --crop as a box on the picture as it's shown, and back.
enum CropBox {
    static func parse(_ s: String, in size: CGSize) -> CGRect? {
        let t = s.trimmingCharacters(in: .whitespaces)
        guard !t.isEmpty, t != "auto", size.width > 0, size.height > 0 else { return nil }
        if t.contains(":") {
            let p = t.split(separator: ":").compactMap { Double($0) }
            guard p.count == 2, p[0] > 0, p[1] > 0 else { return nil }
            return largest(ratio: p[0] / p[1], in: size)
        }
        let parts = t.split(separator: "+").map(String.init)
        let wh = parts[0].lowercased().split(separator: "x").compactMap { Double($0) }
        guard wh.count == 2 else { return nil }
        if parts.count == 3, let x = Double(parts[1]), let y = Double(parts[2]) {
            return CGRect(x: x, y: y, width: wh[0], height: wh[1])
        }
        return CGRect(x: (size.width - wh[0]) / 2, y: (size.height - wh[1]) / 2, width: wh[0], height: wh[1])
    }

    static func largest(ratio: Double, in size: CGSize) -> CGRect {
        var w = size.width, h = size.width / ratio
        if h > size.height { h = size.height; w = h * ratio }
        return CGRect(x: (size.width - w) / 2, y: (size.height - h) / 2, width: w, height: h)
    }

    /// Whole pixels, and even ones for video, which encoders need.
    static func format(_ r: CGRect, in size: CGSize, even: Bool) -> String {
        func round(_ v: CGFloat) -> Int {
            let i = Int(v.rounded())
            return even ? i - i % 2 : i
        }
        let w = max(round(r.width), 2), h = max(round(r.height), 2)
        let x = max(0, min(round(r.minX), Int(size.width) - w))
        let y = max(0, min(round(r.minY), Int(size.height) - h))
        if abs(Double(w) - size.width) <= 2 && abs(Double(h) - size.height) <= 2 { return "" }
        return "\(w)x\(h)+\(x)+\(y)"
    }
}

/// "90", "1:30", "1:02:03.5", "30s" or "-5" (from the end), in seconds.
func parseTime(_ s: String, duration: Double) -> Double? {
    var t = s.trimmingCharacters(in: .whitespaces).lowercased()
    guard !t.isEmpty else { return nil }
    let negative = t.hasPrefix("-")
    if negative { t.removeFirst() }
    if t.hasSuffix("s") { t.removeLast() }
    var total = 0.0
    for part in t.split(separator: ":") {
        guard let v = Double(part) else { return nil }
        total = total * 60 + v
    }
    return negative ? max(0, duration - total) : total
}

// MARK: - Editor

/// One file, every change at once, and one conv run to make it: the video, audio and
/// image editors. The preview shows the turn, the flip, the crop and the part kept.
struct EditorView: View {
    @Environment(AppModel.self) private var model
    @Bindable var job: ConvertJob
    let media: Media
    var focus: EditorSection?
    @State private var preview: MediaPreview
    @State private var aspect: Double?
    @State private var dropHover = false

    init(job: ConvertJob, media: Media, focus: EditorSection?, preview: MediaPreview = MediaPreview()) {
        _job = Bindable(wrappedValue: job)
        self.media = media
        self.focus = focus
        _preview = State(initialValue: preview)
    }

    /// The picture, the bar and the status line on their own, for drawing off screen.
    var stageOnly: some View {
        VStack(spacing: 0) {
            if let file {
                stage(file).frame(height: 420)
                if timed { timeline }
                Divider()
                statusBar(file)
            }
        }
    }

    private var file: FileItem? { job.files.first }
    private var turn: Turn { Turn(rotate: job.rotate, flip: job.flip) }
    private var isVideoLike: Bool { media == .video }
    private var timed: Bool { media != .image }

    var body: some View {
        HSplitView {
            VStack(spacing: 0) {
                if let file {
                    stage(file)
                    if timed { timeline }
                    Divider()
                    statusBar(file)
                } else {
                    empty
                }
            }
            .frame(minWidth: 420, maxWidth: .infinity, maxHeight: .infinity)
            .overlay {
                if dropHover {
                    RoundedRectangle(cornerRadius: 10)
                        .strokeBorder(Color.accentColor, style: StrokeStyle(lineWidth: 3, dash: [8]))
                        .padding(6)
                        .allowsHitTesting(false)
                }
            }
            .dropDestination(for: URL.self) { urls, _ in
                guard let first = urls.first else { return false }
                model.open(job.tool, with: [first])
                return true
            } isTargeted: { dropHover = $0 }

            inspector
                .frame(minWidth: 290, idealWidth: 320, maxWidth: 380)
        }
        .navigationTitle(file.map { "\(job.tool.title): \($0.displayName)" } ?? job.tool.title)
        .toolbar { toolbar }
        .onChange(of: job.settingsKey) { job.optionsChanged() }
        .task(id: file?.url) { await preview.load(file?.url, media: media) }
        .onChange(of: job.speed) { preview.rate = Float(Double(job.speed.replacingOccurrences(of: "x", with: "")) ?? 1) }
        .onChange(of: rangeKey) { preview.range = keptRange }
        .onChange(of: preview.duration) { preview.range = keptRange }
        .onDisappear { preview.stop() }
    }

    // MARK: Toolbar

    @ToolbarContentBuilder
    private var toolbar: some ToolbarContent {
        ToolbarItemGroup(placement: .primaryAction) {
            Button { choose() } label: { Label("Open", systemImage: "folder") }
                .help("Open a file to edit (⌘O)")
            Button {
                Task { if let first = await model.pasteURLs().first { model.open(job.tool, with: [first]) } }
            } label: { Label("Paste", systemImage: "doc.on.clipboard") }
                .help("Edit a file copied in Finder, or a copied picture")
            ThenMenu(tools: nextTools, disabled: job.readyCount == 0 || job.isRunning) { next in
                Task { await model.chain(job, to: next) }
            }
            if job.isRunning {
                Button("Stop") { job.cancel() }
            } else {
                Button("Save") { job.runAll() }
                    .buttonStyle(.borderedProminent)
                    .disabled(job.readyCount == 0)
                    .keyboardShortcut("s")
                    .help("Make the file with every change (⌘S)")
            }
        }
    }

    private var nextTools: [ToolID] {
        model.nextTools(after: job.tool, making: job.fitting.compactMap(\.plannedOutput))
    }

    // MARK: Stage

    private func stage(_ file: FileItem) -> some View {
        GeometryReader { geo in
            let avail = CGSize(width: max(geo.size.width - 40, 50), height: max(geo.size.height - 40, 50))
            let shown = turn.shown(preview.size == .zero ? CGSize(width: 16, height: 9) : preview.size)
            let k = min(avail.width / shown.width, avail.height / shown.height)
            let disp = CGSize(width: shown.width * k, height: shown.height * k)
            ZStack {
                Rectangle().fill(Color(white: 0.11))
                if media == .audio {
                    audioCard(file)
                } else {
                    ZStack {
                        picture(disp: disp)
                        if job.crop == "auto" {
                            Text(isVideoLike ? "conv finds the black bars and cuts them away when it saves."
                                             : "conv finds the plain border and trims it when it saves.")
                                .font(.callout).padding(8)
                                .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 6))
                        } else if cropRect != nil {
                            CropOverlay(rect: cropBinding(k: k, shown: shown), bounds: disp, aspect: aspect)
                        }
                    }
                    .frame(width: disp.width, height: disp.height)
                }
            }
            .frame(width: geo.size.width, height: geo.size.height)
        }
        .frame(minHeight: 260)
    }

    @ViewBuilder
    private func picture(disp: CGSize) -> some View {
        let raw = turn.sideways ? CGSize(width: disp.height, height: disp.width) : disp
        Group {
            if let player = preview.player, preview.size != .zero {
                VideoSurface(player: player)
            } else if preview.isGIF, let url = preview.url {
                AnimatedImage(url: url)
            } else if let image = preview.image {
                Image(nsImage: image).resizable()
            } else {
                ProgressView()
            }
        }
        .frame(width: raw.width, height: raw.height)
        .rotationEffect(.degrees(Double(turn.quarters) * 90))
        .scaleEffect(x: turn.flipH ? -1 : 1, y: turn.flipV ? -1 : 1)
        .frame(width: disp.width, height: disp.height)
    }

    private func audioCard(_ file: FileItem) -> some View {
        VStack(spacing: 12) {
            TileIcon(symbol: "waveform", color: Category.audio.color, size: 72)
            Text(file.displayName).font(.title3.weight(.semibold)).foregroundStyle(.white)
            Text(TrimClock.clock(preview.duration)).foregroundStyle(.white.opacity(0.7)).monospacedDigit()
        }
    }

    // MARK: Crop

    private var shownSize: CGSize { turn.shown(preview.size) }
    private var cropRect: CGRect? { CropBox.parse(job.crop, in: shownSize) }

    /// The crop box in the stage's points, kept in conv's pixels.
    private func cropBinding(k: CGFloat, shown: CGSize) -> Binding<CGRect> {
        Binding(
            get: {
                let r = cropRect ?? CGRect(origin: .zero, size: shown)
                return CGRect(x: r.minX * k, y: r.minY * k, width: r.width * k, height: r.height * k)
            },
            set: { r in
                let px = CGRect(x: r.minX / k, y: r.minY / k, width: r.width / k, height: r.height / k)
                let s = CropBox.format(px, in: shown, even: isVideoLike)
                // A box the size of the whole picture is no crop, but keep the box showing.
                job.crop = s.isEmpty ? CropBox.format(px.insetBy(dx: 2, dy: 2), in: shown, even: isVideoLike) : s
            }
        )
    }

    private func setAspect(_ ratio: Double?) {
        aspect = ratio
        guard shownSize != .zero else { return }
        let r: CGRect
        if let ratio {
            r = CropBox.largest(ratio: ratio, in: shownSize)
        } else {
            r = cropRect ?? CGRect(origin: .zero, size: shownSize).insetBy(dx: shownSize.width * 0.05, dy: shownSize.height * 0.05)
        }
        job.crop = CropBox.format(r, in: shownSize, even: isVideoLike)
        if job.crop.isEmpty {
            // The picture already has that shape.
            job.crop = CropBox.format(r.insetBy(dx: 2, dy: 2), in: shownSize, even: isVideoLike)
        }
    }

    // MARK: Timeline

    private var rangeKey: String { job.start + "|" + job.end + "|" + job.duration }

    private var keptRange: ClosedRange<Double> {
        let d = preview.duration
        let s = min(max(parseTime(job.start, duration: d) ?? 0, 0), d)
        var e = parseTime(job.end, duration: d) ?? d
        if let len = parseTime(job.duration, duration: d), job.end.isEmpty { e = s + len }
        e = min(max(e, s), d)
        return s...e
    }

    private var timeline: some View {
        HStack(spacing: 12) {
            Button { preview.togglePlay() } label: {
                Image(systemName: preview.playing ? "pause.fill" : "play.fill").frame(width: 18)
            }
            .buttonStyle(.borderless)
            .keyboardShortcut(.space, modifiers: [])
            .disabled(preview.player == nil)
            .help(preview.player == nil ? "This file can't play here; trim by the numbers." : "Play the part kept (Space)")
            TrimBar(
                duration: preview.duration, range: keptRange,
                current: preview.player == nil ? nil : preview.current,
                setStart: { t in
                    job.duration = ""
                    job.start = t <= 0.0005 ? "" : TrimClock.stamp(t)
                },
                setEnd: { t in
                    job.duration = ""
                    job.end = t >= preview.duration - 0.0005 ? "" : TrimClock.stamp(t)
                },
                seek: { preview.seek($0) }
            )
            .why(job.reason("--start"))
            Text("\(TrimClock.clock(preview.current)) / \(TrimClock.clock(preview.duration))")
                .font(.system(.callout, design: .monospaced))
                .foregroundStyle(.secondary)
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
    }

    // MARK: Status

    @ViewBuilder
    private func statusBar(_ file: FileItem) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 8) {
                switch file.status {
                case .running:
                    ProgressView().controlSize(.small)
                    Text("Making it…").foregroundStyle(.secondary)
                case .exists(let m):
                    Label(m, systemImage: "doc.on.doc.fill").foregroundStyle(.orange)
                    Button("Replace") { job.retry(file, extra: ["-y"]) }.controlSize(.small)
                case .needsConfirm(let m):
                    Label(m, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.orange).lineLimit(2)
                    Button("Save Anyway") { job.retry(file, extra: ["--yes"]) }.controlSize(.small)
                case .failed(let m):
                    Label(m, systemImage: "xmark.octagon.fill").foregroundStyle(.red).lineLimit(3)
                case .done(let out, let size):
                    savedLine(out, size: size)
                case .idle:
                    if let saved = job.lastOutputs.first {
                        savedLine(saved, size: nil)
                        Text("·").foregroundStyle(.tertiary)
                    }
                    if let p = file.problem {
                        Label(p, systemImage: "exclamationmark.circle").foregroundStyle(.secondary).lineLimit(2)
                    } else if let out = file.plannedOutput {
                        Label("Saves as \(out.pathExtension.isEmpty ? out.lastPathComponent + "/" : out.lastPathComponent)",
                              systemImage: "arrow.turn.down.right")
                            .foregroundStyle(.secondary)
                    } else {
                        Text("Checking…").foregroundStyle(.tertiary)
                    }
                }
                Spacer(minLength: 0)
            }
            .font(.callout)
            ForEach(file.warnings.prefix(3), id: \.self) { w in
                Text(w).font(.caption).foregroundStyle(.orange).lineLimit(2)
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.bar)
    }

    @ViewBuilder
    private func savedLine(_ out: URL, size: Int64?) -> some View {
        Label {
            Text("Saved \(out.lastPathComponent)") + Text(size.map { "  \(bytes($0))" } ?? "").foregroundStyle(.secondary)
        } icon: {
            Image(systemName: "checkmark.circle.fill").foregroundStyle(.green)
        }
        Button("Show") { NSWorkspace.shared.activateFileViewerSelecting([out]) }.controlSize(.small)
        OpenInMenu(output: out, from: job.tool)
    }

    // MARK: Empty

    private var empty: some View {
        VStack(spacing: 14) {
            ToolIcon(tool: job.tool, size: 52)
            Text(job.preset.dropTitle).font(.title2.weight(.semibold))
            Text(job.preset.dropDetail)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 400)
            Button("Choose…") { choose() }.controlSize(.large)
        }
        .padding()
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .contentShape(Rectangle())
    }

    private func choose() {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = switch media {
        case .video: [.movie, .gif]
        case .audio: [.audio]
        case .image: [.image]
        }
        if panel.runModal() == .OK, let url = panel.url { model.open(job.tool, with: [url]) }
    }

    // MARK: Inspector

    private var inspector: some View {
        ScrollViewReader { proxy in
            Form {
                if timed { trimSection.id(EditorSection.trim) }
                if media != .audio {
                    cropSection.id(EditorSection.crop)
                    turnSection.id(EditorSection.turn)
                }
                if timed { speedSection.id(EditorSection.speed) }
                if timed { soundSection.id(EditorSection.sound) }
                if media == .video { subtitlesSection.id(EditorSection.subtitles) }
                if media != .audio { sizeSection.id(EditorSection.size) }
                formatSection
                privacySection
                saveSection
            }
            .formStyle(.grouped)
            .onAppear {
                if let focus { proxy.scrollTo(focus, anchor: .top) }
            }
        }
    }

    private func header(_ title: String, _ section: EditorSection?) -> some View {
        Text(title)
            .foregroundStyle(section != nil && section == focus ? Color.accentColor : .primary)
    }

    private var trimSection: some View {
        Section {
            HStack {
                TextField("Start", text: $job.start, prompt: Text("0:00"))
                Button { job.duration = ""; job.start = TrimClock.stamp(preview.current) } label: {
                    Image(systemName: "chevron.left.to.line")
                }
                .help("Start here, at the playhead")
                .disabled(preview.player == nil)
            }
            .why(job.reason("--start"))
            HStack {
                TextField("End", text: $job.end, prompt: Text("the end"))
                Button { job.duration = ""; job.end = TrimClock.stamp(preview.current) } label: {
                    Image(systemName: "chevron.right.to.line")
                }
                .help("End here, at the playhead")
                .disabled(preview.player == nil)
            }
            .why(job.reason("--end"))
            if !job.start.isEmpty || !job.end.isEmpty {
                LabeledContent("Keeps", value: TrimClock.clock(keptRange.upperBound - keptRange.lowerBound))
            }
        } header: {
            header("Trim", .trim)
        } footer: {
            Text("Drag the ends of the bar under the picture, or type 90, 1:30 or -30 for the last 30 seconds.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    private var cropSection: some View {
        Section {
            let options: [(String, Double?)] = [("Free", nil), ("1:1", 1), ("9:16", 9.0 / 16), ("16:9", 16.0 / 9),
                                                ("4:5", 0.8), ("4:3", 4.0 / 3)]
            LazyVGrid(columns: [GridItem(.adaptive(minimum: 56), spacing: 6)], spacing: 6) {
                chip("None", on: job.crop.isEmpty) { aspect = nil; job.crop = "" }
                ForEach(options, id: \.0) { o in
                    chip(o.0, on: !job.crop.isEmpty && job.crop != "auto" && aspect == o.1) { setAspect(o.1) }
                }
                chip(isVideoLike ? "Bars" : "Border", on: job.crop == "auto") { aspect = nil; job.crop = "auto" }
                    .help(isVideoLike ? "Cut away black bars around the picture" : "Trim a plain border")
            }
            .why(job.reason("--crop"))
            if let r = cropRect {
                LabeledContent("Box", value: "\(Int(r.width)) × \(Int(r.height)) at \(Int(r.minX)), \(Int(r.minY))")
            }
        } header: {
            header("Crop", .crop)
        } footer: {
            Text(job.reason("--crop") ?? "Drag the box or its corners on the picture. A shape keeps the box that shape.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    private func chip(_ title: String, on: Bool, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Text(title)
                .font(.callout)
                .frame(maxWidth: .infinity)
                .padding(.vertical, 4)
                .background(on ? Color.accentColor : Color.primary.opacity(0.07), in: RoundedRectangle(cornerRadius: 6))
                .foregroundStyle(on ? .white : .primary)
        }
        .buttonStyle(.plain)
    }

    private var turnSection: some View {
        Section {
            HStack(spacing: 8) {
                turnButton("rotate.left", "Turn left") { $0.quarters -= 1 }
                turnButton("rotate.right", "Turn right") { $0.quarters += 1 }
                turnButton("arrow.left.and.right.righttriangle.left.righttriangle.right", "Mirror, left to right") { t in
                    if t.flipV { t.flipV = false; t.quarters += 2; t.flipH = false } else { t.flipH.toggle() }
                }
                .why(job.reason("--flip"))
                turnButton("arrow.up.and.down.righttriangle.up.righttriangle.down", "Flip top to bottom") { t in
                    if t.flipH { t.flipH = false; t.quarters += 2; t.flipV = false } else { t.flipV.toggle() }
                }
                .why(job.reason("--flip"))
                Spacer()
                if !job.rotate.isEmpty || !job.flip.isEmpty {
                    Button("Reset") { job.rotate = ""; job.flip = ""; job.crop = job.crop == "auto" ? "auto" : "" }
                        .controlSize(.small)
                }
            }
            .why(job.reason("--rotate"))
            Text(turn.summary).font(.callout).foregroundStyle(.secondary)
        } header: {
            header("Turn and Flip", .turn)
        } footer: {
            if let r = job.reason("--rotate") {
                Text(r).font(.caption).foregroundStyle(.secondary)
            }
        }
    }

    private func turnButton(_ symbol: String, _ help: String, _ change: @escaping (inout Turn) -> Void) -> some View {
        Button {
            var t = turn
            change(&t)
            t.normalize()
            job.rotate = t.rotate
            job.flip = t.flip
            // A box drawn on the old picture doesn't fit the turned one.
            if job.crop != "auto" { job.crop = "" }
        } label: {
            Image(systemName: symbol).font(.title3).frame(width: 34, height: 26)
        }
        .help(help)
    }

    private var speedSection: some View {
        Section {
            PresetField(
                "Speed", value: $job.speed,
                presets: [("Normal", ""), ("Half (0.5×)", "0.5"), ("0.75×", "0.75"), ("1.25×", "1.25"),
                          ("1.5×", "1.5"), ("Double (2×)", "2"), ("4×", "4")],
                placeholder: "3"
            )
            .why(job.reason("--speed"))
            if !job.speed.isEmpty && job.enabled("--speed") && job.applies("--shift-pitch") {
                Toggle("Let the pitch change too", isOn: $job.shiftPitch)
            }
        } header: {
            header("Speed", .speed)
        } footer: {
            Text("The sound keeps its pitch unless you say otherwise. The preview plays at this speed.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    private var soundSection: some View {
        Section {
            if media == .video {
                Toggle("Mute", isOn: $job.mute).why(job.reason("--mute"))
            }
            Picker("Background noise", selection: $job.denoise) {
                Text("Leave it").tag("")
                Text("Take it out").tag("normal")
                Text("Take more out (strong)").tag("strong")
            }
            .why(job.reason("--denoise"))
            if media == .video {
                FilePickRow(title: "New sound", file: $job.audioFile, types: [.audio, .audiovisualContent])
                    .why(job.reason("--audio"))
                if job.audioFile != nil && job.enabled("--audio") {
                    Toggle("Mix it under the video's own sound", isOn: $job.mix).why(job.reason("--mix"))
                    if job.mix {
                        PresetField("How loud", value: $job.mixLevel,
                                    presets: [("A quarter as loud", ""), ("Half as loud", "50%"), ("As loud", "100%")],
                                    placeholder: "40%")
                    }
                }
            }
        } header: {
            header("Sound", .sound)
        } footer: {
            if media == .video, let r = job.reason("--mute"), !job.mute {
                Text(r).font(.caption).foregroundStyle(.secondary)
            }
        }
    }

    private var subtitlesSection: some View {
        Section {
            FilePickRow(title: "Subtitles", file: $job.subtitlesFile,
                        types: ["srt", "vtt", "ass"].compactMap { UTType(filenameExtension: $0) })
                .why(job.reason("--subtitles"))
        } header: {
            header("Subtitles", .subtitles)
        } footer: {
            Text(job.reason("--subtitles") ?? "Drawn into the picture: white with an outline, near the bottom. A trim or speed change moves them with it.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    private var sizeSection: some View {
        Section {
            PresetField(
                "Resize", value: $job.resize,
                presets: [("Original size", ""), ("Half (50%)", "50%"), ("Quarter (25%)", "25%"),
                          ("1920 wide", "1920x"), ("1280 wide", "1280x"), ("640 wide", "640x")],
                placeholder: "1600x900"
            )
            .why(job.reason("--resize"))
            if media == .video {
                PresetField(
                    "Keep under", value: $job.maxSize,
                    presets: [("No limit", ""), ("8 MB", "8mb"), ("10 MB", "10mb"), ("25 MB", "25mb"), ("50 MB", "50mb")],
                    placeholder: "50 MB"
                )
                .why(job.reason("--max-size"))
                PresetField(
                    "Frame rate", value: $job.fps,
                    presets: [("As is", ""), ("Up to 30", "30"), ("Up to 24", "24"), ("Up to 15", "15")],
                    placeholder: "12"
                )
                .why(job.reason("--fps"))
            } else {
                Toggle("Set quality", isOn: $job.useQuality).why(job.reason("--quality"))
                if job.useQuality && job.enabled("--quality") {
                    Slider(value: $job.quality, in: 30...100, step: 1) { Text("Quality \(Int(job.quality))") }
                }
            }
        } header: {
            header("Size", .size)
        } footer: {
            if let r = [job.reason("--resize"), job.reason("--max-size")].compactMap({ $0 }).first, media == .video || job.reason("--resize") != nil {
                Text(r).font(.caption).foregroundStyle(.secondary)
            }
        }
    }

    private var formatSection: some View {
        Section("Save as") {
            Picker("Format", selection: Binding(get: { job.format }, set: { job.choose(format: $0) })) {
                Text("Same format").tag("same")
                if !job.targets.isEmpty { Divider() }
                ForEach(job.targets, id: \.self) { Text($0.uppercased()).tag($0) }
            }
            .disabled(job.fitting.isEmpty)
            if !job.ignoredOptions.isEmpty {
                Label("\(job.ignoredOptions.joined(separator: ", ")) won't be used for \(job.format == "same" ? "this file" : job.format.uppercased()).",
                      systemImage: "info.circle")
                    .font(.caption).foregroundStyle(.orange)
            }
        }
    }

    private var privacySection: some View {
        Section("Privacy") {
            Toggle(media == .image ? "Remove location and camera details" : "Remove location and other metadata",
                   isOn: $job.strip)
                .why(job.reason("--strip-metadata"))
        }
    }

    private var saveSection: some View {
        Section {
            Picker("Save in", selection: Binding(
                get: { job.outputFolder == nil ? 0 : 1 },
                set: { if $0 == 0 { job.outputFolder = nil } else { chooseFolder() } }
            )) {
                Text(job.fromChain ? "Next to \(model.chain?.origin.lastPathComponent ?? "the original")" : "Next to the original").tag(0)
                Text(job.outputFolder?.lastPathComponent ?? "Choose…").tag(1)
            }
            .pickerStyle(.radioGroup)
            .labelsHidden()
        } header: {
            Text("Save in")
        } footer: {
            VStack(alignment: .leading, spacing: 4) {
                if job.fromChain, let origin = model.chain?.origin {
                    Text("This is a step in a chain that started with \(origin.lastPathComponent).")
                        .font(.caption).foregroundStyle(.secondary)
                }
                Text(job.commandPreview.isEmpty ? "" : job.commandPreview)
                    .font(.system(.caption2, design: .monospaced))
                    .foregroundStyle(.tertiary)
                    .textSelection(.enabled)
            }
        }
    }

    private func chooseFolder() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.prompt = "Save Here"
        if panel.runModal() == .OK, let url = panel.url { job.outputFolder = url }
    }
}

// MARK: - Pieces

enum TrimClock {
    /// Seconds with millisecond precision, the form conv's --start and --end take.
    static func stamp(_ t: Double) -> String { String(format: "%.3f", max(t, 0)) }

    static func clock(_ t: Double) -> String {
        guard t.isFinite, t >= 0 else { return "0:00" }
        let ms = Int((t * 1000).rounded())
        let (m, s, frac) = (ms / 60000, (ms / 1000) % 60, (ms % 1000) / 10)
        return String(format: "%d:%02d.%02d", m, s, frac)
    }
}

/// The video, through a bare player layer so the editor's own controls drive it.
struct VideoSurface: NSViewRepresentable {
    let player: AVPlayer

    final class Surface: NSView {
        let layerView = AVPlayerLayer()
        override init(frame: NSRect) {
            super.init(frame: frame)
            wantsLayer = true
            layerView.videoGravity = .resizeAspect
            layer?.addSublayer(layerView)
        }
        required init?(coder: NSCoder) { nil }
        override func layout() {
            super.layout()
            layerView.frame = bounds
        }
    }

    func makeNSView(context: Context) -> Surface {
        let v = Surface()
        v.layerView.player = player
        return v
    }

    func updateNSView(_ v: Surface, context: Context) {
        if v.layerView.player !== player { v.layerView.player = player }
    }
}

/// A GIF, playing.
struct AnimatedImage: NSViewRepresentable {
    let url: URL
    func makeNSView(context: Context) -> NSImageView {
        let v = NSImageView()
        v.animates = true
        v.imageScaling = .scaleAxesIndependently
        v.image = NSImage(contentsOf: url)
        return v
    }
    func updateNSView(_ v: NSImageView, context: Context) {}
}

/// A crop box over the picture: drag inside to move it, drag a corner to size it.
struct CropOverlay: View {
    @Binding var rect: CGRect
    let bounds: CGSize
    let aspect: Double?
    @State private var start: CGRect?

    enum Corner: CaseIterable { case topLeft, topRight, bottomLeft, bottomRight }

    var body: some View {
        ZStack(alignment: .topLeading) {
            Path { p in
                p.addRect(CGRect(origin: .zero, size: bounds))
                p.addRect(rect)
            }
            .fill(Color.black.opacity(0.5), style: FillStyle(eoFill: true))
            .allowsHitTesting(false)

            grid
                .frame(width: rect.width, height: rect.height)
                .contentShape(Rectangle())
                .offset(x: rect.minX, y: rect.minY)
                .gesture(move)

            ForEach(Corner.allCases, id: \.self) { c in
                RoundedRectangle(cornerRadius: 2)
                    .fill(.white)
                    .frame(width: 14, height: 14)
                    .shadow(radius: 1)
                    .position(point(c, of: rect))
                    .gesture(resize(c))
            }
        }
        .frame(width: bounds.width, height: bounds.height, alignment: .topLeading)
        .coordinateSpace(name: "crop")
    }

    private var grid: some View {
        ZStack {
            Rectangle().strokeBorder(.white, lineWidth: 1.5)
            Path { p in
                for i in 1...2 {
                    let x = rect.width * CGFloat(i) / 3, y = rect.height * CGFloat(i) / 3
                    p.move(to: CGPoint(x: x, y: 0)); p.addLine(to: CGPoint(x: x, y: rect.height))
                    p.move(to: CGPoint(x: 0, y: y)); p.addLine(to: CGPoint(x: rect.width, y: y))
                }
            }
            .stroke(.white.opacity(0.35), lineWidth: 0.5)
        }
    }

    private func point(_ c: Corner, of r: CGRect) -> CGPoint {
        switch c {
        case .topLeft: CGPoint(x: r.minX, y: r.minY)
        case .topRight: CGPoint(x: r.maxX, y: r.minY)
        case .bottomLeft: CGPoint(x: r.minX, y: r.maxY)
        case .bottomRight: CGPoint(x: r.maxX, y: r.maxY)
        }
    }

    private var move: some Gesture {
        DragGesture(minimumDistance: 1, coordinateSpace: .named("crop"))
            .onChanged { v in
                let s = start ?? rect
                if start == nil { start = rect }
                let x = min(max(s.minX + v.translation.width, 0), bounds.width - s.width)
                let y = min(max(s.minY + v.translation.height, 0), bounds.height - s.height)
                rect = CGRect(x: x, y: y, width: s.width, height: s.height)
            }
            .onEnded { _ in start = nil }
    }

    private func resize(_ c: Corner) -> some Gesture {
        DragGesture(minimumDistance: 0, coordinateSpace: .named("crop"))
            .onChanged { v in
                let s = start ?? rect
                if start == nil { start = rect }
                // The corner across from this one stays put.
                let anchor: CGPoint = switch c {
                case .topLeft: CGPoint(x: s.maxX, y: s.maxY)
                case .topRight: CGPoint(x: s.minX, y: s.maxY)
                case .bottomLeft: CGPoint(x: s.maxX, y: s.minY)
                case .bottomRight: CGPoint(x: s.minX, y: s.minY)
                }
                let p0 = point(c, of: s)
                let p = CGPoint(x: min(max(p0.x + v.translation.width, 0), bounds.width),
                                y: min(max(p0.y + v.translation.height, 0), bounds.height))
                var w = max(abs(p.x - anchor.x), 16), h = max(abs(p.y - anchor.y), 16)
                if let aspect {
                    if w / h > aspect { w = h * aspect } else { h = w / aspect }
                }
                let left = c == .topLeft || c == .bottomLeft, top = c == .topLeft || c == .topRight
                let x = left ? anchor.x - w : anchor.x
                let y = top ? anchor.y - h : anchor.y
                rect = CGRect(x: x, y: y, width: w, height: h).intersection(CGRect(origin: .zero, size: bounds))
            }
            .onEnded { _ in start = nil }
    }
}

/// The bar under the picture: the part kept between two handles, and the playhead.
struct TrimBar: View {
    let duration: Double
    let range: ClosedRange<Double>
    let current: Double?
    let setStart: (Double) -> Void
    let setEnd: (Double) -> Void
    let seek: (Double) -> Void

    var body: some View {
        GeometryReader { geo in
            let w = geo.size.width
            let x = { (t: Double) -> CGFloat in duration > 0 ? CGFloat(t / duration) * w : 0 }
            let t = { (px: CGFloat) -> Double in duration > 0 ? min(max(Double(px / w) * duration, 0), duration) : 0 }
            ZStack(alignment: .leading) {
                RoundedRectangle(cornerRadius: 5).fill(Color.primary.opacity(0.1))
                    .contentShape(Rectangle())
                    .gesture(DragGesture(minimumDistance: 0, coordinateSpace: .named("trimbar")).onChanged { seek(t($0.location.x)) })
                RoundedRectangle(cornerRadius: 5)
                    .fill(Color.accentColor.opacity(0.3))
                    .overlay(RoundedRectangle(cornerRadius: 5).strokeBorder(Color.accentColor, lineWidth: 2))
                    .frame(width: max(x(range.upperBound) - x(range.lowerBound), 2))
                    .offset(x: x(range.lowerBound))
                    .allowsHitTesting(false)
                if let current {
                    Rectangle().fill(.white).frame(width: 2).shadow(radius: 1)
                        .offset(x: x(current) - 1)
                        .allowsHitTesting(false)
                }
                handle.offset(x: x(range.lowerBound) - 5)
                    .gesture(DragGesture(minimumDistance: 0, coordinateSpace: .named("trimbar")).onChanged { v in
                        setStart(min(t(v.location.x), range.upperBound - 0.1))
                    })
                handle.offset(x: x(range.upperBound) - 5)
                    .gesture(DragGesture(minimumDistance: 0, coordinateSpace: .named("trimbar")).onChanged { v in
                        setEnd(max(t(v.location.x), range.lowerBound + 0.1))
                    })
            }
            .coordinateSpace(name: "trimbar")
        }
        .frame(height: 30)
    }

    private var handle: some View {
        RoundedRectangle(cornerRadius: 3)
            .fill(Color.accentColor)
            .frame(width: 10, height: 30)
            .overlay(Capsule().fill(.white.opacity(0.8)).frame(width: 2, height: 12))
            .contentShape(Rectangle())
    }
}
