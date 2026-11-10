import AppKit
import Observation
import SwiftUI

/// `conv ascii`: the preview is conv's own text from `--json`.
@Observable
final class AsciiModel {
    var conv: URL?
    var input: URL?
    var width: Double = 100
    var style = "ascii"
    var customChars = " .:oO@"
    var background = "dark"
    var contrast: Double = 0
    var brightness: Double = 0

    private(set) var art = ""
    private(set) var warnings: [String] = []
    private(set) var problem: String?
    private(set) var saved: URL?
    private(set) var working = false

    private var task: Task<Void, Never>?

    /// Everything but the input and output.
    private var lookArgs: [String] {
        let chars = style == "custom" ? customChars : style
        return ["--width", String(Int(width)), "--chars", chars, "--bg", background,
                "--contrast", String(Int(contrast)), "--brightness", String(Int(brightness))]
    }

    var command: String {
        guard let input else { return "" }
        let quoted = lookArgs.map { $0.contains(" ") || $0.isEmpty ? "'\($0)'" : $0 }
        return (["conv", "ascii"] + quoted + ["'\(input.lastPathComponent)'"]).joined(separator: " ")
    }

    func changed() {
        task?.cancel()
        saved = nil
        guard let conv, let input else {
            art = ""
            problem = nil
            return
        }
        let args = ["ascii", "--json", "--no-install", "--no-color"] + lookArgs + ["--", input.path]
        task = Task { @MainActor in
            try? await Task.sleep(for: .milliseconds(150))
            guard !Task.isCancelled else { return }
            working = true
            defer { working = false }
            guard let out = try? await Conv.run(conv, args), !Task.isCancelled else { return }
            guard let resp = out.decode(Response.self) else {
                problem = Conv.stderrMessage(out.stderr)
                return
            }
            let r = resp.results?.first
            if let r, r.ok, let text = r.text {
                art = text
                warnings = r.warnings ?? []
                problem = nil
            } else {
                art = ""
                problem = (r?.error ?? resp.error).map(AppModel.friendly) ?? "conv couldn't draw that."
            }
        }
    }

    @MainActor
    func copy() {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(art, forType: .string)
    }

    @MainActor
    func save() {
        guard let conv, let input else { return }
        let base = input.deletingPathExtension().lastPathComponent + "-ascii"
        guard let url = askWhereToSave(name: base + ".png", types: ["png", "txt", "html", "jpg", "webp"])
        else { return }
        // The save panel has already asked about replacing, so -y is the user's answer.
        let args = ["ascii", "--json", "--no-install", "-y"] + lookArgs + ["--", input.path, url.path]
        task = Task { @MainActor in
            working = true
            defer { working = false }
            guard let out = try? await Conv.run(conv, args) else { return }
            let resp = out.decode(Response.self)
            if let r = resp?.results?.first, r.ok {
                saved = r.output.map(URL.init(fileURLWithPath:)) ?? url
                problem = nil
            } else {
                problem = (resp?.results?.first?.error ?? resp?.error).map(AppModel.friendly)
                    ?? Conv.stderrMessage(out.stderr)
            }
        }
    }

    private struct Response: Decodable {
        struct Result: Decodable {
            var ok: Bool
            var text: String?
            var output: String?
            var warnings: [String]?
            var error: ConvError?
        }
        var ok: Bool
        var results: [Result]?
        var error: ConvError?
    }
}

struct AsciiView: View {
    @Bindable var ascii: AsciiModel
    let paste: () async -> [URL]
    @State private var dropHover = false

    var body: some View {
        HSplitView {
            preview
                .frame(minWidth: 420, maxWidth: .infinity, maxHeight: .infinity)
            controls
                .frame(minWidth: 280, idealWidth: 300, maxWidth: 360)
        }
        .navigationTitle("ASCII Art")
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                Button { Task { if let first = await paste().first { ascii.input = first } } } label: {
                    Label("Paste", systemImage: "doc.on.clipboard")
                }
                .help("Use a copied picture, screenshot or file")
                Button("Copy Text") { ascii.copy() }.disabled(ascii.art.isEmpty)
                Button("Save…") { ascii.save() }
                    .buttonStyle(.borderedProminent)
                    .disabled(ascii.art.isEmpty)
            }
        }
        .onChange(of: key) { ascii.changed() }
    }

    private var key: String {
        [ascii.input?.path ?? "", "\(Int(ascii.width))", ascii.style, ascii.customChars,
         ascii.background, "\(Int(ascii.contrast))", "\(Int(ascii.brightness))"].joined(separator: "|")
    }

    private var dark: Bool { ascii.background == "dark" }

    @ViewBuilder
    private var preview: some View {
        ZStack {
            (dark ? Color.black : Color.white)
            if ascii.input == nil {
                VStack(spacing: 12) {
                    ToolIcon(tool: .ascii, size: 52)
                    Text("Drop a picture, GIF or video").font(.title3.weight(.semibold))
                    Button("Choose…") { choose() }
                }
                .foregroundStyle(dark ? .white.opacity(0.7) : .black.opacity(0.6))
            } else if let problem = ascii.problem {
                Label(problem, systemImage: "exclamationmark.triangle")
                    .foregroundStyle(.orange)
                    .padding()
            } else {
                GeometryReader { geo in
                    ScrollView([.horizontal, .vertical]) {
                        Text(ascii.art)
                            .font(.system(size: fontSize(for: geo.size), design: .monospaced))
                            .foregroundStyle(dark ? Color.white : Color.black)
                            .textSelection(.enabled)
                            .fixedSize()
                            .padding(12)
                            .frame(minWidth: geo.size.width, minHeight: geo.size.height)
                    }
                }
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
            ascii.input = first
            return true
        } isTargeted: { dropHover = $0 }
    }

    /// Fit the art's width to the pane; monospaced glyphs are about 0.6 em wide.
    private func fontSize(for size: CGSize) -> CGFloat {
        let cols = max(ascii.art.split(separator: "\n").map(\.count).max() ?? Int(ascii.width), 10)
        return min(max((size.width - 24) / (CGFloat(cols) * 0.62), 4), 16)
    }

    private var controls: some View {
        Form {
            Section("Picture") {
                if let input = ascii.input {
                    LabeledContent("File") {
                        Text(Clipboard.isPasted(input) ? "Pasted picture" : input.lastPathComponent)
                            .lineLimit(1).truncationMode(.middle)
                    }
                }
                Button(ascii.input == nil ? "Choose…" : "Choose Another…") { choose() }
            }
            Section {
                Picker("Style", selection: $ascii.style) {
                    Text("Characters").tag("ascii")
                    Text("Blocks").tag("blocks")
                    Text("Braille (most detail)").tag("braille")
                    Text("My own characters").tag("custom")
                }
                if ascii.style == "custom" {
                    TextField("From least ink to most", text: $ascii.customChars)
                        .font(.system(.body, design: .monospaced))
                }
                Picker("Shown on", selection: $ascii.background) {
                    Text("Dark").tag("dark")
                    Text("Light").tag("light")
                }
                .pickerStyle(.segmented)
                LabeledContent("Width") {
                    HStack {
                        Slider(value: $ascii.width, in: 20...240, step: 2)
                        Text("\(Int(ascii.width))").monospacedDigit().frame(width: 32, alignment: .trailing)
                    }
                }
                LabeledContent("Contrast") {
                    Slider(value: $ascii.contrast, in: -100...100, step: 5)
                }
                LabeledContent("Brightness") {
                    Slider(value: $ascii.brightness, in: -100...100, step: 5)
                }
                Button("Reset") {
                    ascii.contrast = 0
                    ascii.brightness = 0
                    ascii.width = 100
                }
            } header: {
                Text("Look")
            } footer: {
                if ascii.input == nil {
                    Text("Pick a picture first.").font(.caption).foregroundStyle(.secondary)
                }
            }
            .disabled(ascii.input == nil)
            if !ascii.warnings.isEmpty {
                Section {
                    ForEach(ascii.warnings, id: \.self) {
                        Text($0).font(.caption).foregroundStyle(.orange)
                    }
                }
            }
            Section {
                if let saved = ascii.saved {
                    Button("Saved \(saved.lastPathComponent). Show in Finder") {
                        NSWorkspace.shared.activateFileViewerSelecting([saved])
                    }
                    .buttonStyle(.link)
                }
                Text(ascii.command.isEmpty ? "Pick a picture to see the command." : ascii.command)
                    .font(.system(.caption, design: .monospaced))
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
            } header: {
                Text("Command")
            }
        }
        .formStyle(.grouped)
    }

    private func choose() {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = [.image, .movie, .gif]
        if panel.runModal() == .OK { ascii.input = panel.url }
    }
}
