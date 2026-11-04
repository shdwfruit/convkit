import AVKit
import Observation
import SwiftUI
import UniformTypeIdentifiers

/// The window version of `conv trim`: mark clips on a player, and each clip is
/// written as `conv FILE --start T --end T` would write it.
@Observable
final class TrimModel {
    struct Clip: Identifiable, Equatable {
        enum Keep: String, CaseIterable {
            case both = "Picture and sound"
            case sound = "Sound only (m4a)"
        }
        enum Status: Equatable {
            case idle, running
            case done(URL)
            case failed(String)
            case exists
        }
        let id = UUID()
        var start: Double
        var end: Double
        var keep: Keep = .both
        var status: Status = .idle
    }

    var conv: URL?
    private(set) var file: URL?
    private(set) var player: AVPlayer?
    private(set) var duration: Double = 0
    private(set) var hasVideo = false
    var clips: [Clip] = []
    /// A start mark waiting for its end.
    var openStart: Double?
    /// Target format for clips with a picture; empty keeps the file's own.
    var format = ""
    private(set) var isRunning = false
    /// Picture formats this file's clips can be written as.
    private(set) var targets: [String] = []

    @MainActor
    func load(_ url: URL) {
        file = url
        clips = []
        openStart = nil
        let item = AVPlayerItem(url: url)
        player = AVPlayer(playerItem: item)
        Task { @MainActor in
            let asset = item.asset
            duration = (try? await asset.load(.duration)).map(CMTimeGetSeconds) ?? 0
            hasVideo = !((try? await asset.loadTracks(withMediaType: .video)) ?? []).isEmpty
            if let conv, let out = try? await Conv.run(conv, ["capabilities", url.pathExtension.lowercased(), "--json"]),
               let caps = out.decode(CapabilitiesResponse.self) {
                let audio: Set = ["mp3", "m4a", "wav", "flac", "opus", "ogg", "aac", "aiff", "wma"]
                targets = (caps.targets ?? []).map(\.to).filter { !audio.contains($0) }
            }
        }
    }

    var now: Double { player.map { CMTimeGetSeconds($0.currentTime()) } ?? 0 }

    @MainActor
    func markStart() { openStart = now }

    @MainActor
    func markEnd() {
        let end = now
        let start = openStart ?? 0
        guard end > start else { return }
        clips.append(Clip(start: start, end: end, keep: hasVideo ? .both : .sound))
        openStart = nil
    }

    @MainActor
    func seek(_ t: Double) {
        player?.seek(to: CMTime(seconds: t, preferredTimescale: 600), toleranceBefore: .zero, toleranceAfter: .zero)
    }

    /// The extension a clip is written as, or nil for the file's own.
    func targetExt(for clip: Clip) -> String? {
        clip.keep == .sound ? "m4a" : (format.isEmpty ? nil : format)
    }

    func rangeArgs(for clip: Clip) -> [String] {
        ["--start", Self.stamp(clip.start), "--end", Self.stamp(clip.end)]
    }

    /// conv names a same-format cut for its range (talk-1m02s-1m10s.mp4) but a cut
    /// into another format after the file alone. Clips are named for their range
    /// either way, like `conv trim` does: ask conv for the range name, swap the extension.
    @MainActor
    func args(for clip: Clip, conv: URL) async -> [String]? {
        guard let file else { return nil }
        guard let ext = targetExt(for: clip) else { return [file.path] + rangeArgs(for: clip) }
        guard let out = try? await Conv.run(conv, [file.path] + rangeArgs(for: clip) + ["--dry-run", "--json"]),
              let plan = out.decode(DryRunResponse.self)?.plans?.first?.plan?.output
        else { return nil }
        let named = URL(fileURLWithPath: plan).deletingPathExtension().appendingPathExtension(ext)
        return [file.path, named.path] + rangeArgs(for: clip)
    }

    @MainActor
    func writeAll() {
        guard let conv else { return }
        isRunning = true
        Task { @MainActor in
            for i in clips.indices where !isDone(clips[i]) {
                await write(i, conv: conv, extra: [])
            }
            isRunning = false
        }
    }

    @MainActor
    func replace(_ clip: Clip) {
        guard let conv, let i = clips.firstIndex(of: clip) else { return }
        isRunning = true
        Task { @MainActor in
            await write(i, conv: conv, extra: ["-y"])
            isRunning = false
        }
    }

    private func isDone(_ c: Clip) -> Bool {
        if case .done = c.status { return true }
        return false
    }

    @MainActor
    private func write(_ i: Int, conv: URL, extra: [String]) async {
        clips[i].status = .running
        guard let args = await args(for: clips[i], conv: conv) else {
            clips[i].status = .failed("conv couldn't plan this clip.")
            return
        }
        guard let out = try? await Conv.run(conv, args + ["--json", "--no-install"] + extra),
              let resp = out.decode(RunResponse.self)
        else {
            clips[i].status = .failed("conv didn't answer.")
            return
        }
        let r = resp.results?.first
        if let r, r.ok, let path = r.output {
            clips[i].status = .done(URL(fileURLWithPath: path))
        } else if let err = r?.error ?? resp.error {
            clips[i].status = err.code == "output_exists" ? .exists : .failed(AppModel.friendly(err))
        } else {
            clips[i].status = .failed("conv didn't say what went wrong.")
        }
    }

    /// Seconds with millisecond precision, the form conv's --start and --end take.
    static func stamp(_ t: Double) -> String { String(format: "%.3f", max(t, 0)) }

    static func clock(_ t: Double) -> String {
        guard t.isFinite else { return "0:00" }
        let ms = Int((t * 1000).rounded())
        let (m, s, frac) = (ms / 60000, (ms / 1000) % 60, (ms % 1000) / 10)
        return String(format: "%d:%02d.%02d", m, s, frac)
    }
}

struct PlayerView: NSViewRepresentable {
    let player: AVPlayer
    func makeNSView(context: Context) -> AVPlayerView {
        let v = AVPlayerView()
        v.controlsStyle = .floating
        v.showsFrameSteppingButtons = true
        v.player = player
        return v
    }
    func updateNSView(_ v: AVPlayerView, context: Context) { v.player = player }
}

struct TrimView: View {
    @Bindable var trim: TrimModel
    let tool: ToolID
    let paste: () async -> [URL]
    private var audioOnly: Bool { tool == .trimAudio }
    @State private var dropHover = false
    @State private var rejected: String?

    /// Loads a file the tool can play, and says so when it can't.
    private func take(_ url: URL) {
        let type = UTType(filenameExtension: url.pathExtension)
        guard let type, type.conforms(to: audioOnly ? .audio : .audiovisualContent) else {
            rejected = "\(url.lastPathComponent) isn't \(audioOnly ? "an audio file" : "a video or audio file")."
            return
        }
        trim.load(url)
    }

    var body: some View {
        Group {
            if let player = trim.player, let file = trim.file {
                VStack(spacing: 0) {
                    PlayerView(player: player)
                        .frame(minHeight: 260)
                    Divider()
                    marks(file)
                    Divider()
                    clipList
                }
            } else {
                VStack(spacing: 14) {
                    ToolIcon(tool: tool, size: 52)
                    Text(audioOnly ? "Drop a recording" : "Drop a video").font(.title2.weight(.semibold))
                    Text("Play it, mark where each clip starts and ends, and write them all.")
                        .foregroundStyle(.secondary)
                    Button("Choose…") { choose() }.controlSize(.large)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .overlay {
            if dropHover {
                RoundedRectangle(cornerRadius: 10)
                    .strokeBorder(Color.accentColor, style: StrokeStyle(lineWidth: 3, dash: [8]))
                    .padding(6)
            }
        }
        .dropDestination(for: URL.self) { urls, _ in
            guard let first = urls.first else { return false }
            take(first)
            return true
        } isTargeted: { dropHover = $0 }
        .alert("\(tool.title) can't play this", isPresented: Binding(
            get: { rejected != nil }, set: { if !$0 { rejected = nil } }
        )) {
            Button("OK") { rejected = nil }
        } message: {
            Text(rejected ?? "")
        }
        .navigationTitle(tool.title)
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                Button { choose() } label: { Label("Open", systemImage: "folder") }
                Button { Task { if let first = await paste().first { take(first) } } } label: {
                    Label("Paste", systemImage: "doc.on.clipboard")
                }
                .help(audioOnly ? "Cut an audio file copied in Finder" : "Cut a video copied in Finder")
                Button(trim.clips.count > 1 ? "Write \(trim.clips.count) Clips" : "Write Clip") { trim.writeAll() }
                    .buttonStyle(.borderedProminent)
                    .disabled(trim.clips.isEmpty || trim.isRunning)
            }
        }
    }

    private func marks(_ file: URL) -> some View {
        HStack(spacing: 10) {
            Button {
                trim.markStart()
            } label: {
                Label("Mark Start", systemImage: "chevron.left.to.line")
            }
            .keyboardShortcut("i", modifiers: [])
            .help("I")
            Button {
                trim.markEnd()
            } label: {
                Label("Mark End", systemImage: "chevron.right.to.line")
            }
            .keyboardShortcut("o", modifiers: [])
            .help("O. Without a start mark, the clip starts at the beginning.")
            if let s = trim.openStart {
                Text("Start at \(TrimModel.clock(s)). Now mark the end.")
                    .font(.callout).foregroundStyle(.secondary)
                Button("Drop") { trim.openStart = nil }.buttonStyle(.link)
            }
            Spacer()
            if trim.hasVideo {
                Picker("Clips as", selection: $trim.format) {
                    Text("Same format").tag("")
                    ForEach(trim.targets, id: \.self) { Text($0.uppercased()).tag($0) }
                }
                .frame(width: 200)
            }
        }
        .padding(10)
    }

    private var clipList: some View {
        List {
            if trim.clips.isEmpty {
                Text("No clips yet. Press I at a clip's start and O at its end.")
                    .foregroundStyle(.secondary)
            }
            ForEach($trim.clips) { $clip in
                HStack(spacing: 12) {
                    Button {
                        trim.seek(clip.start)
                    } label: {
                        Text("\(TrimModel.clock(clip.start)) – \(TrimModel.clock(clip.end))")
                            .monospacedDigit()
                    }
                    .buttonStyle(.link)
                    .help("Jump to the clip's start")
                    Text(TrimModel.clock(clip.end - clip.start)).foregroundStyle(.secondary).monospacedDigit()
                    if trim.hasVideo {
                        Picker("Keep", selection: $clip.keep) {
                            ForEach(TrimModel.Clip.Keep.allCases, id: \.self) { Text($0.rawValue).tag($0) }
                        }
                        .labelsHidden()
                        .frame(width: 170)
                    }
                    Spacer()
                    status(clip)
                }
            }
            .onDelete { trim.clips.remove(atOffsets: $0) }
        }
        .frame(minHeight: 140)
    }

    @ViewBuilder
    private func status(_ clip: TrimModel.Clip) -> some View {
        switch clip.status {
        case .idle:
            Button {
                trim.clips.removeAll { $0.id == clip.id }
            } label: {
                Image(systemName: "xmark.circle.fill").foregroundStyle(.tertiary)
            }
            .buttonStyle(.plain)
        case .running:
            ProgressView().controlSize(.small)
        case .done(let url):
            Button("Show \(url.lastPathComponent)") { NSWorkspace.shared.activateFileViewerSelecting([url]) }
                .buttonStyle(.link)
        case .failed(let message):
            Text(message).font(.caption).foregroundStyle(.red).lineLimit(2)
        case .exists:
            HStack {
                Text("Already exists").font(.caption).foregroundStyle(.orange)
                Button("Replace") { trim.replace(clip) }.controlSize(.small).disabled(trim.isRunning)
            }
        }
    }

    private func choose() {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = audioOnly ? [.audio] : [.movie]
        if panel.runModal() == .OK, let url = panel.url { trim.load(url) }
    }
}
