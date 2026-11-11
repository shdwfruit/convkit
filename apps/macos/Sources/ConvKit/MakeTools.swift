import AppKit
import Observation
import SwiftUI
import UniformTypeIdentifiers

/// A run that writes one file, as most of conv's commands report it.
private struct OneFileResponse: Decodable {
    struct Result: Decodable {
        var ok: Bool
        var output: String?
        var bytes: Int64?
        var warnings: [String]?
        var notes: [String]?
        var error: ConvError?
    }
    var ok: Bool
    var results: [Result]?
    var error: ConvError?

    var result: Result? { results?.first }
    var problem: String? { (result?.error ?? error).map(AppModel.friendly) }
    var messages: [String] { (result?.warnings ?? []) + (result?.notes ?? []) }
}

private func scratchFolder(_ name: String) -> URL {
    let dir = appTemp.appendingPathComponent("convkit-\(name)", isDirectory: true)
    try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    return dir
}

private func dropZone(_ tool: ToolID, title: String, detail: String, choose: @escaping () -> Void) -> some View {
    VStack(spacing: 14) {
        ToolIcon(tool: tool, size: 52)
        Text(title).font(.title2.weight(.semibold))
        Text(detail).foregroundStyle(.secondary).multilineTextAlignment(.center).frame(maxWidth: 420)
        Button("Choose…", action: choose).controlSize(.large)
    }
    .padding()
    .frame(maxWidth: .infinity, maxHeight: .infinity)
}

private func pickImage() -> URL? {
    let panel = NSOpenPanel()
    panel.allowedContentTypes = [.image]
    return panel.runModal() == .OK ? panel.url : nil
}

// MARK: - Text to Speech

/// `conv speak`: typed text or a document, read aloud into a sound file.
@Observable
final class SpeakModel {
    struct Voice: Decodable, Hashable {
        var name: String
        var language: String?
        var quality: String?
    }

    var conv: URL?
    var text = ""
    private(set) var file: URL?
    var useFile = false
    var voice = ""
    var rate = ""
    var format = "m4a"
    let formats = ["m4a", "mp3", "wav", "aiff"]

    private(set) var voices: [Voice] = []
    private(set) var problem: String?
    private(set) var notes: [String] = []
    private(set) var saved: URL?
    private(set) var working = false
    private(set) var playing = false
    private var sound: NSSound?

    @MainActor
    func loadVoices() async {
        guard let conv, voices.isEmpty,
              let out = try? await Conv.run(conv, ["speak", "--voices", "--json"]) else { return }
        struct R: Decodable { var voices: [Voice]? }
        let lang = Locale.current.language.languageCode?.identifier ?? "en"
        let all = out.decode(R.self)?.voices ?? []
        // Your language first, the better voices first within it.
        let rank = ["premium": 0, "enhanced": 1]
        voices = all.sorted { a, b in
            let la = a.language?.hasPrefix(lang) == true, lb = b.language?.hasPrefix(lang) == true
            if la != lb { return la }
            let qa = rank[a.quality ?? ""] ?? 2, qb = rank[b.quality ?? ""] ?? 2
            if qa != qb { return qa < qb }
            return a.name.localizedCompare(b.name) == .orderedAscending
        }
    }

    @MainActor
    func load(_ url: URL) {
        file = url
        useFile = true
        saved = nil
    }

    var ready: Bool { !working && (useFile ? file != nil : !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty) }

    private func run(to out: URL) -> QuickRun {
        var a = ["speak", "--json", "--no-install", "-y"]
        if !voice.isEmpty { a += ["--voice", voice] }
        if !rate.isEmpty { a += ["--rate", rate] }
        if useFile, let file { return QuickRun(args: a + ["-f", file.path, out.path]) }
        return QuickRun(args: a + ["-", out.path], stdin: text)
    }

    @MainActor
    private func speak(to out: URL) async -> Bool {
        guard let conv else { return false }
        let r = run(to: out)
        working = true
        defer { working = false }
        guard let o = try? await Conv.run(conv, r.args, stdin: r.stdin.map { Data($0.utf8) }),
              let resp = o.decode(OneFileResponse.self) else {
            problem = "conv didn't answer."
            return false
        }
        problem = resp.problem
        notes = resp.messages
        return resp.result?.ok == true
    }

    /// Reads it into a scratch file and plays that, so what you hear is what Save writes.
    @MainActor
    func listen() {
        if playing { sound?.stop(); playing = false; return }
        let out = scratchFolder("speak").appendingPathComponent("listen.m4a")
        Task { @MainActor in
            guard await speak(to: out), let s = NSSound(contentsOf: out, byReference: false) else { return }
            sound = s
            playing = true
            s.play()
            let length = s.duration
            try? await Task.sleep(for: .seconds(length + 0.2))
            if sound === s { playing = false }
        }
    }

    @MainActor
    func save() {
        let stem = useFile ? (file?.deletingPathExtension().lastPathComponent ?? "speech") : "speech"
        guard let url = askWhereToSave(name: "\(stem).\(format)", types: [format]) else { return }
        Task { @MainActor in
            if await speak(to: url) { saved = url }
        }
    }
}

struct SpeakView: View {
    @Bindable var speak: SpeakModel

    var body: some View {
        HSplitView {
            VStack(spacing: 0) {
                Picker("Read", selection: $speak.useFile) {
                    Text("Text").tag(false)
                    Text("File").tag(true)
                }
                .pickerStyle(.segmented).labelsHidden().fixedSize().padding(8)
                if speak.useFile {
                    if let file = speak.file {
                        VStack(spacing: 10) {
                            Image(nsImage: NSWorkspace.shared.icon(forFile: file.path)).resizable().frame(width: 64, height: 64)
                            Text(file.lastPathComponent).font(.headline)
                            Button("Choose Another…") { choose() }
                        }
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                    } else {
                        dropZone(.speak, title: "Drop a document to read aloud",
                                 detail: "Text, Markdown, HTML, Word, OpenDocument or RTF, read as its text.", choose: choose)
                    }
                } else {
                    TextEditor(text: $speak.text)
                        .font(.title3)
                        .scrollContentBackground(.hidden)
                        .padding(10)
                        .overlay(alignment: .topLeading) {
                            if speak.text.isEmpty {
                                Text("Type or paste what to read aloud").font(.title3).foregroundStyle(.tertiary)
                                    .padding(.horizontal, 15).padding(.vertical, 10).allowsHitTesting(false)
                            }
                        }
                }
            }
            .frame(minWidth: 360, maxWidth: .infinity, maxHeight: .infinity)
            .dropDestination(for: URL.self) { urls, _ in
                guard let f = urls.first, f.isFileURL, ToolID.speakExts.contains(fileExt(f)) else { return false }
                speak.load(f)
                return true
            }

            Form {
                Section {
                    Picker("Voice", selection: $speak.voice) {
                        Text("The best one you have").tag("")
                        Divider()
                        ForEach(speak.voices, id: \.self) { v in
                            Text("\(v.name)  \(v.language ?? "")\(v.quality == "standard" || v.quality == nil ? "" : ", \(v.quality!)")")
                                .tag(v.name)
                        }
                    }
                    Picker("Pace", selection: $speak.rate) {
                        Text("The voice's own").tag("")
                        Text("Slower (140 a minute)").tag("140")
                        Text("Faster (220 a minute)").tag("220")
                        Text("Much faster (300 a minute)").tag("300")
                    }
                    Picker("Save as", selection: $speak.format) {
                        ForEach(speak.formats, id: \.self) { Text($0.uppercased()).tag($0) }
                    }
                } footer: {
                    Text("macOS's own voices, on this Mac. A Premium or Enhanced voice sounds far more natural; System Settings > Accessibility > Spoken Content has them.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                Section {
                    if speak.working {
                        HStack { ProgressView().controlSize(.small); Text("Reading…").foregroundStyle(.secondary) }
                    }
                    if let p = speak.problem {
                        Label(p, systemImage: "exclamationmark.triangle").foregroundStyle(.orange)
                    }
                    ForEach(speak.notes, id: \.self) { Text($0).font(.caption).foregroundStyle(.secondary) }
                    if let s = speak.saved {
                        Button("Saved \(s.lastPathComponent). Show in Finder") { NSWorkspace.shared.activateFileViewerSelecting([s]) }
                            .buttonStyle(.link)
                    }
                }
            }
            .formStyle(.grouped)
            .frame(minWidth: 300, idealWidth: 330, maxWidth: 400)
        }
        .navigationTitle("Text to Speech")
        .task { await speak.loadVoices() }
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                Button { speak.listen() } label: {
                    Label(speak.playing ? "Stop" : "Listen", systemImage: speak.playing ? "stop.fill" : "play.fill")
                }
                .disabled(!speak.ready && !speak.playing)
                Button("Save…") { speak.save() }
                    .buttonStyle(.borderedProminent)
                    .disabled(!speak.ready)
            }
        }
    }

    private func choose() {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = ToolID.speakExts.compactMap { UTType(filenameExtension: $0) }
        if panel.runModal() == .OK, let url = panel.url { speak.load(url) }
    }
}

// MARK: - Beautify

/// `conv beautify`: the preview is conv's own output, written to a scratch folder.
@Observable
final class BeautifyModel {
    var conv: URL?
    private(set) var input: URL?
    var background = "aurora"
    var custom = "#1e1e2e"
    var padding = ""
    var radius = ""
    var shadow = true

    private(set) var preview: NSImage?
    private(set) var size = ""
    private(set) var problem: String?
    private(set) var notes: [String] = []
    private(set) var saved: URL?
    private(set) var working = false
    private var task: Task<Void, Never>?

    static let backgrounds = ["aurora", "sunset", "candy", "ocean", "mint", "night"]

    @MainActor
    func load(_ url: URL) {
        input = url
        changed()
    }

    private func options() -> [String] {
        var a = ["--bg", background == "custom" ? custom : background]
        if let p = Int(padding) { a += ["--padding", String(p)] }
        if let r = Int(radius) { a += ["--radius", String(r)] }
        if !shadow { a.append("--no-shadow") }
        return a
    }

    func changed() {
        task?.cancel()
        saved = nil
        guard let conv, let input else { return }
        let out = scratchFolder("beautify").appendingPathComponent("preview-\(UUID().uuidString).png")
        let args = ["beautify", "--json", "-y"] + options() + [input.path, out.path]
        task = Task { @MainActor in
            try? await Task.sleep(for: .milliseconds(200))
            guard !Task.isCancelled else { return }
            working = true
            defer { working = false; try? FileManager.default.removeItem(at: out) }
            guard let o = try? await Conv.run(conv, args), !Task.isCancelled else { return }
            struct R: Decodable {
                struct B: Decodable { var width: Int?; var height: Int? }
                struct Item: Decodable { var ok: Bool; var beautify: B?; var notes: [String]?; var warnings: [String]?; var error: ConvError? }
                var ok: Bool
                var results: [Item]?
                var error: ConvError?
            }
            guard let r = o.decode(R.self) else { problem = Conv.stderrMessage(o.stderr); return }
            if let item = r.results?.first, item.ok {
                preview = (try? Data(contentsOf: out)).flatMap(NSImage.init(data:))
                size = item.beautify.map { "\($0.width ?? 0) × \($0.height ?? 0)" } ?? ""
                notes = (item.warnings ?? []) + (item.notes ?? [])
                problem = nil
            } else {
                preview = nil
                problem = (r.results?.first?.error ?? r.error).map(AppModel.friendly) ?? "conv couldn't do that."
            }
        }
    }

    @MainActor
    func save() {
        guard let conv, let input else { return }
        let stem = input.deletingPathExtension().lastPathComponent + "-beautified"
        guard let url = askWhereToSave(name: stem + ".png", types: ["png", "jpg", "webp"]) else { return }
        Task { @MainActor in
            working = true
            defer { working = false }
            guard let o = try? await Conv.run(conv, ["beautify", "--json", "-y"] + options() + [input.path, url.path]),
                  let r = o.decode(OneFileResponse.self) else { problem = "conv didn't answer."; return }
            if r.ok, r.result?.ok == true { saved = url } else { problem = r.problem ?? "conv couldn't do that." }
        }
    }

    @MainActor
    func copyImage() {
        guard let preview else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.writeObjects([preview])
    }
}

struct BeautifyView: View {
    @Bindable var beautify: BeautifyModel
    let paste: () async -> [URL]

    var body: some View {
        HSplitView {
            Group {
                if beautify.input == nil {
                    dropZone(.beautify, title: "Drop a screenshot",
                             detail: "It goes on a gradient with padding, rounded corners and a soft shadow. The screenshot itself is never scaled, so a Retina one stays sharp.",
                             choose: { if let u = pickImage() { beautify.load(u) } })
                } else {
                    ZStack {
                        Rectangle().fill(.background.secondary)
                        if let img = beautify.preview {
                            Image(nsImage: img).resizable().aspectRatio(contentMode: .fit).padding(20).draggable(img)
                        } else if let p = beautify.problem {
                            Label(p, systemImage: "exclamationmark.triangle").foregroundStyle(.orange).padding()
                        }
                        if beautify.working {
                            ProgressView().controlSize(.small)
                                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topTrailing).padding(10)
                        }
                    }
                }
            }
            .frame(minWidth: 400, maxWidth: .infinity, maxHeight: .infinity)
            .dropDestination(for: URL.self) { urls, _ in
                guard let f = urls.first(where: \.isFileURL) else { return false }
                beautify.load(f)
                return true
            }

            Form {
                Section {
                    Picker("Background", selection: $beautify.background) {
                        ForEach(BeautifyModel.backgrounds, id: \.self) { Text($0.capitalized).tag($0) }
                        Divider()
                        Text("Transparent").tag("transparent")
                        Text("Your own…").tag("custom")
                    }
                    if beautify.background == "custom" {
                        TextField("Colours", text: $beautify.custom, prompt: Text("#1e1e2e, or #f00,#00f for a gradient"))
                    }
                }
                Section {
                    TextField("Padding", text: $beautify.padding, prompt: Text("8% of its size"))
                    TextField("Corner radius", text: $beautify.radius, prompt: Text("about 1% of its size"))
                    Toggle("Shadow", isOn: $beautify.shadow)
                } footer: {
                    Text("In points, doubled on a Retina screenshot so it looks the same. A window captured with its own shadow keeps its shape.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                Section {
                    if !beautify.size.isEmpty { LabeledContent("Size", value: beautify.size) }
                    ForEach(beautify.notes, id: \.self) { Text($0).font(.caption).foregroundStyle(.secondary) }
                    if let s = beautify.saved {
                        Button("Saved \(s.lastPathComponent). Show in Finder") { NSWorkspace.shared.activateFileViewerSelecting([s]) }
                            .buttonStyle(.link)
                    }
                }
            }
            .formStyle(.grouped)
            .frame(minWidth: 280, idealWidth: 310, maxWidth: 380)
        }
        .navigationTitle("Beautify Screenshot")
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                Button { if let u = pickImage() { beautify.load(u) } } label: { Label("Open", systemImage: "folder") }
                Button { Task { if let f = await paste().first { beautify.load(f) } } } label: {
                    Label("Paste", systemImage: "doc.on.clipboard")
                }
                .help("Use a copied screenshot or picture")
                Button("Copy Image") { beautify.copyImage() }.disabled(beautify.preview == nil)
                Button("Save…") { beautify.save() }.buttonStyle(.borderedProminent).disabled(beautify.preview == nil)
            }
        }
        .onChange(of: [beautify.background, beautify.custom, beautify.padding, beautify.radius, "\(beautify.shadow)"]) {
            beautify.changed()
        }
    }
}

// MARK: - Color palette

/// `conv colors`: a picture's main colours, and the same as code.
@Observable
final class ColorsModel {
    struct Swatch: Decodable, Hashable {
        var hex: String
        var rgb: [Int]
        var hsl: [Int]
        var share: Double
    }

    var conv: URL?
    private(set) var input: URL?
    /// The picture, read once rather than on every redraw.
    private(set) var image: NSImage?
    var count = 6
    var code = ""
    private(set) var swatches: [Swatch] = []
    private(set) var codeText = ""
    private(set) var problem: String?
    private(set) var copied: String?
    private var task: Task<Void, Never>?

    @MainActor
    func load(_ url: URL) {
        input = url
        image = NSImage(contentsOf: url)
        changed()
    }

    func changed() {
        task?.cancel()
        guard let conv, let input else { return }
        let n = String(count), code = code
        task = Task { @MainActor in
            try? await Task.sleep(for: .milliseconds(150))
            guard !Task.isCancelled,
                  let o = try? await Conv.run(conv, ["colors", "--json", "-n", n, input.path]), !Task.isCancelled else { return }
            struct R: Decodable { var ok: Bool; var colors: [Swatch]?; var error: ConvError? }
            guard let r = o.decode(R.self) else { problem = Conv.stderrMessage(o.stderr); return }
            problem = r.error.map(AppModel.friendly)
            // The rows read three numbers from each.
            swatches = (r.colors ?? []).filter { $0.rgb.count == 3 && $0.hsl.count == 3 }
            if code.isEmpty {
                codeText = ""
            } else if let c = try? await Conv.run(conv, ["colors", "-n", n, "--code", code, input.path]) {
                codeText = String(decoding: c.stdout, as: UTF8.self)
            }
        }
    }

    func copy(_ s: String, as label: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(s, forType: .string)
        copied = "Copied \(label)."
    }
}

struct ColorsView: View {
    @Bindable var colors: ColorsModel
    let paste: () async -> [URL]

    var body: some View {
        Group {
            if colors.input != nil {
                HSplitView {
                    Image(nsImage: colors.image ?? NSImage())
                        .resizable().aspectRatio(contentMode: .fit).padding(16)
                        .frame(minWidth: 280, maxWidth: .infinity, maxHeight: .infinity)
                        .background(.background.secondary)
                    ScrollView {
                        VStack(alignment: .leading, spacing: 14) {
                            HStack {
                                Stepper("Colours: \(colors.count)", value: $colors.count, in: 1...16).fixedSize()
                                Spacer()
                                Picker("As code", selection: $colors.code) {
                                    Text("None").tag("")
                                    Text("CSS").tag("css")
                                    Text("SwiftUI").tag("swiftui")
                                    Text("Tailwind 4").tag("tailwind")
                                    Text("Tailwind 3").tag("tailwind3")
                                }
                                .fixedSize()
                            }
                            if let p = colors.problem {
                                Label(p, systemImage: "exclamationmark.triangle").foregroundStyle(.orange)
                            }
                            ForEach(colors.swatches, id: \.self) { s in swatchRow(s) }
                            if !colors.codeText.isEmpty {
                                Text(colors.codeText)
                                    .font(.system(.callout, design: .monospaced))
                                    .textSelection(.enabled)
                                    .padding(10)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                                    .background(.background.secondary, in: RoundedRectangle(cornerRadius: 8))
                            }
                            if let c = colors.copied { Text(c).font(.caption).foregroundStyle(.secondary) }
                        }
                        .padding(16)
                    }
                    .frame(minWidth: 340, idealWidth: 380, maxWidth: 480)
                }
            } else {
                dropZone(.colors, title: "Drop a picture",
                         detail: "Its main colours, the most common first, with their share. Colours a person would call the same count as one.",
                         choose: { if let u = pickImage() { colors.load(u) } })
            }
        }
        .dropDestination(for: URL.self) { urls, _ in
            guard let f = urls.first(where: \.isFileURL) else { return false }
            colors.load(f)
            return true
        }
        .navigationTitle("Color Palette")
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                Button { if let u = pickImage() { colors.load(u) } } label: { Label("Open", systemImage: "folder") }
                Button { Task { if let f = await paste().first { colors.load(f) } } } label: {
                    Label("Paste", systemImage: "doc.on.clipboard")
                }
                .help("Use a copied picture")
                Button(colors.codeText.isEmpty ? "Copy HEX" : "Copy Code") {
                    if colors.codeText.isEmpty {
                        colors.copy(colors.swatches.map(\.hex).joined(separator: "\n"), as: "the HEX values")
                    } else {
                        colors.copy(colors.codeText, as: "the code")
                    }
                }
                .buttonStyle(.borderedProminent)
                .disabled(colors.swatches.isEmpty)
            }
        }
        .onChange(of: colors.count) { colors.changed() }
        .onChange(of: colors.code) { colors.changed() }
    }

    private func swatchRow(_ s: ColorsModel.Swatch) -> some View {
        HStack(spacing: 12) {
            RoundedRectangle(cornerRadius: 8)
                .fill(Color(red: Double(s.rgb[0]) / 255, green: Double(s.rgb[1]) / 255, blue: Double(s.rgb[2]) / 255))
                .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(.quaternary))
                .frame(width: 56, height: 40)
            VStack(alignment: .leading, spacing: 2) {
                Text(s.hex.uppercased()).font(.system(.body, design: .monospaced).weight(.medium))
                Text("rgb(\(s.rgb.map(String.init).joined(separator: ", ")))  hsl(\(s.hsl[0]), \(s.hsl[1])%, \(s.hsl[2])%)")
                    .font(.system(.caption, design: .monospaced)).foregroundStyle(.secondary)
            }
            Spacer()
            Text("\(Int((s.share * 100).rounded()))%").foregroundStyle(.secondary)
            Button("Copy") { colors.copy(s.hex, as: s.hex) }.controlSize(.small)
        }
    }
}

// MARK: - Wi-Fi

/// `conv wifi`: a saved network's password, shown only when asked for, or a QR code
/// that joins it. macOS asks before conv can read the keychain.
@Observable
final class WifiModel {
    var conv: URL?
    private(set) var networks: [String] = []
    var name = ""
    private(set) var security = ""
    private(set) var password: String?
    private(set) var notes: [String] = []
    private(set) var problem: String?
    private(set) var qr: NSImage?
    private(set) var saved: URL?
    private(set) var working = false

    @MainActor
    func loadNetworks() async {
        guard let conv, networks.isEmpty,
              let out = try? await Conv.run(conv, ["wifi", "--list", "--json"]) else { return }
        struct R: Decodable { var networks: [String]? ; var error: ConvError? }
        let r = out.decode(R.self)
        networks = r?.networks ?? []
        problem = r?.error.map(AppModel.friendly)
    }

    func reset() {
        password = nil
        security = ""
        qr = nil
        saved = nil
        problem = nil
        notes = []
    }

    /// Last, after `--`, so a name that starts with - isn't read as a flag.
    private var nameArg: [String] { name.isEmpty ? [] : ["--", name] }

    @MainActor
    func show() {
        guard let conv else { return }
        let asked = name
        Task { @MainActor in
            working = true
            defer { working = false }
            guard let out = try? await Conv.run(conv, ["wifi", "--json", "--show-password"] + nameArg) else { return }
            // Another network was picked while macOS asked; this answer is for the old one.
            guard name == asked else { return }
            struct R: Decodable {
                struct N: Decodable { var name: String?; var security: String?; var password: String? }
                var ok: Bool
                var network: N?
                var notes: [String]?
                var error: ConvError?
            }
            guard let r = out.decode(R.self) else { problem = Conv.stderrMessage(out.stderr); return }
            problem = r.error.map(AppModel.friendly)
            notes = r.notes ?? []
            security = r.network?.security ?? ""
            password = r.ok ? (r.network?.password ?? "") : nil
        }
    }

    func copyPassword() {
        guard let password else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(password, forType: .string)
    }

    @MainActor
    func makeQR() {
        guard let conv,
              let url = askWhereToSave(name: "\(name.isEmpty ? "wifi" : name)-wifi.png", types: ["png", "svg", "pdf"])
        else { return }
        let asked = name
        Task { @MainActor in
            working = true
            defer { working = false }
            let out = try? await Conv.run(conv, ["wifi", "--qr", url.path, "--json", "-y"] + nameArg)
            guard name == asked else { return }
            guard let r = out?.decode(OneFileResponse.self) else { problem = "conv didn't answer."; return }
            guard r.ok, r.result?.ok == true else { problem = r.problem ?? "conv couldn't make the code."; return }
            saved = url
            notes = r.messages
            qr = NSImage(contentsOf: url)
        }
    }
}

struct WifiView: View {
    @Bindable var wifi: WifiModel
    @State private var reveal = false

    var body: some View {
        Form {
            Section {
                Picker("Network", selection: $wifi.name) {
                    Text("The one this Mac is on").tag("")
                    Divider()
                    ForEach(wifi.networks, id: \.self) { Text($0).tag($0) }
                }
            } footer: {
                Text("macOS asks you to allow this, with your login password, before conv can read the keychain. The password is shown only here.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Section {
                HStack {
                    Button("Show Password") { reveal = false; wifi.show() }.disabled(wifi.working)
                    Button("Make QR Code…") { wifi.makeQR() }.disabled(wifi.working)
                    if wifi.working { ProgressView().controlSize(.small) }
                }
                if let p = wifi.problem {
                    Label(p, systemImage: "exclamationmark.triangle").foregroundStyle(.orange)
                }
                if let pw = wifi.password {
                    if !wifi.security.isEmpty { LabeledContent("Security", value: wifi.security) }
                    LabeledContent("Password") {
                        HStack {
                            Text(pw.isEmpty ? "None, it's open" : reveal ? pw : String(repeating: "•", count: min(pw.count, 16)))
                                .font(.system(.body, design: .monospaced))
                                .textSelection(.enabled)
                            Button(reveal ? "Hide" : "Reveal") { reveal.toggle() }.controlSize(.small)
                            Button("Copy") { wifi.copyPassword() }.controlSize(.small).disabled(pw.isEmpty)
                        }
                    }
                }
                ForEach(wifi.notes, id: \.self) { Text($0).font(.caption).foregroundStyle(.secondary) }
                if let img = wifi.qr {
                    Image(nsImage: img).resizable().interpolation(.none).aspectRatio(contentMode: .fit).frame(width: 220, height: 220)
                }
                if let s = wifi.saved {
                    Button("Saved \(s.lastPathComponent). Show in Finder") { NSWorkspace.shared.activateFileViewerSelecting([s]) }
                        .buttonStyle(.link)
                }
            }
        }
        .formStyle(.grouped)
        .frame(maxWidth: 620)
        .frame(maxWidth: .infinity)
        .navigationTitle("Wi-Fi Password")
        .task { await wifi.loadNetworks() }
        .onChange(of: wifi.name) { wifi.reset(); reveal = false }
        // Don't keep a password around once its page is gone.
        .onDisappear { wifi.reset() }
    }
}
