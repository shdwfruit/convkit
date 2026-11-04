import AppKit
import Observation
import SwiftUI

/// `conv fmt`: type, paste or open JSON, YAML or TOML, and see conv's formatted
/// version (or where it's broken) as you go.
@Observable
final class DataModel {
    struct Problem: Equatable {
        var message: String
        var hint: String?
        var line: Int?
        var column: Int?
        var snippet: String?
        var caret: Int?
    }

    var conv: URL?
    var text = ""
    private(set) var source: URL?
    /// "" tells the language from the text.
    var from = ""
    /// "" keeps the text's own language.
    var to = ""
    /// "2", "4" or "tab".
    var indent = "2"
    var minify = false
    var sortKeys = false
    var lenient = false

    private(set) var output = ""
    private(set) var inputLanguage: String?
    private(set) var outputLanguage: String?
    private(set) var problem: Problem?
    private(set) var notes: [String] = []
    private(set) var saved: URL?
    private(set) var working = false
    private var task: Task<Void, Never>?

    static let languages = ["json", "yaml", "toml", "csv"]

    /// The language the result is written in, as far as is known.
    var outLanguage: String? { to.isEmpty ? (outputLanguage ?? inputLanguage ?? (from.isEmpty ? nil : from)) : to }
    var readsCSV: Bool { (from.isEmpty ? inputLanguage : from) == "csv" }

    /// Why a knob is greyed out, or nil when conv uses it.
    func reason(_ knob: String) -> String? {
        let out = outLanguage
        switch knob {
        case "minify":
            return out == nil || out == "json" ? nil : "Only JSON packs onto one line."
        case "indent":
            if minify && out == "json" { return "Packed JSON has no indent." }
            return out == nil || out == "json" || out == "yaml" ? nil : "\(out!.uppercased()) has its own layout."
        case "tab":
            return out == nil || out == "json" ? nil : "Only JSON indents with tabs."
        case "keep":
            return readsCSV ? "A CSV is converted, not formatted. Pick JSON or YAML." : nil
        default:
            return nil
        }
    }

    @MainActor
    func load(_ url: URL) {
        saved = nil
        guard let data = try? Data(contentsOf: url), data.count < 20_000_000,
              let s = String(data: data, encoding: .utf8) else {
            problem = Problem(message: "\(url.lastPathComponent) isn't a text file conv can read here.")
            return
        }
        text = s
        source = url
        let ext = url.pathExtension.lowercased()
        from = ext == "yml" ? "yaml" : (Self.languages.contains(ext) ? ext : "")
        if from == "csv" && to.isEmpty { to = "json" }
        changed()
    }

    /// Text from the clipboard, or a file copied in Finder.
    @MainActor
    func paste() {
        let pb = NSPasteboard.general
        if let url = (pb.readObjects(forClasses: [NSURL.self], options: [.urlReadingFileURLsOnly: true]) as? [URL])?.first {
            load(url)
        } else if let s = pb.string(forType: .string) {
            text = s
            source = nil
            changed()
        }
    }

    var args: [String] {
        var a: [String] = []
        if !from.isEmpty { a += ["--from", from] }
        if !to.isEmpty { a += ["--to", to] }
        if minify && reason("minify") == nil {
            a.append("--minify")
        } else if indent == "tab" && reason("tab") == nil {
            a.append("--tab")
        } else if indent != "2" && indent != "tab" && reason("indent") == nil {
            a += ["--indent", indent]
        }
        if sortKeys { a.append("--sort-keys") }
        if lenient { a.append("--lenient") }
        return a
    }

    var command: String {
        let input = source.map { "'\($0.lastPathComponent)'" } ?? "--paste"
        return (["conv", "fmt"] + args + [input]).joined(separator: " ")
    }

    func changed() {
        task?.cancel()
        saved = nil
        guard let conv, !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            output = ""
            problem = nil
            notes = []
            inputLanguage = nil
            outputLanguage = nil
            return
        }
        let text = text
        let args = args
        task = Task { @MainActor in
            try? await Task.sleep(for: .milliseconds(200))
            guard !Task.isCancelled else { return }
            working = true
            defer { working = false }
            // A file with no extension, so conv tells the language from the text
            // unless --from says it.
            let file = FileManager.default.temporaryDirectory.appendingPathComponent("convkit-fmt-input")
            do { try text.write(to: file, atomically: true, encoding: .utf8) } catch { return }
            guard let out = try? await Conv.run(conv, ["fmt", "--json"] + args + ["--", file.path]),
                  !Task.isCancelled else { return }
            guard let resp = out.decode(Response.self) else {
                problem = Problem(message: Conv.stderrMessage(out.stderr))
                output = ""
                return
            }
            let r = resp.results?.first
            inputLanguage = r?.language ?? inputLanguage
            if let r, r.ok, let t = r.text {
                output = t
                outputLanguage = r.output_language
                notes = r.notes ?? []
                problem = nil
            } else if let e = r?.error {
                output = ""
                notes = []
                problem = Problem(message: e.message, hint: e.hint, line: e.line, column: e.column,
                                  snippet: e.snippet?.text, caret: e.snippet?.caret)
            } else {
                output = ""
                problem = Problem(message: resp.error?.message ?? "conv didn't say what went wrong.")
            }
        }
    }

    @MainActor
    func copy() {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(output, forType: .string)
    }

    @MainActor
    func save() {
        let ext = outLanguage ?? "json"
        let base = source?.deletingPathExtension().lastPathComponent ?? "data"
        // Named after the source, with -formatted when it would land on the source's own name.
        let name = source?.pathExtension.lowercased() == ext ? "\(base)-formatted.\(ext)" : "\(base).\(ext)"
        guard let url = askWhereToSave(name: name, types: [ext]) else { return }
        do {
            try output.write(to: url, atomically: true, encoding: .utf8)
            saved = url
        } catch {
            problem = Problem(message: "Couldn't save \(url.lastPathComponent): \(error.localizedDescription)")
        }
    }

    private struct Response: Decodable {
        struct Snippet: Decodable { var caret: Int?; var line: Int?; var text: String? }
        struct Failure: Decodable {
            var code: String?
            var message: String
            var hint: String?
            var line: Int?
            var column: Int?
            var snippet: Snippet?
        }
        struct Result: Decodable {
            var ok: Bool
            var text: String?
            var language: String?
            var output_language: String?
            var notes: [String]?
            var error: Failure?
        }
        var ok: Bool
        var results: [Result]?
        var error: ConvError?
    }
}

struct DataView: View {
    @Bindable var data: DataModel
    @State private var dropHover = false

    var body: some View {
        VStack(spacing: 0) {
            controls
            Divider()
            HSplitView {
                input.frame(minWidth: 320, maxWidth: .infinity, maxHeight: .infinity)
                result.frame(minWidth: 320, maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .navigationTitle("Format JSON")
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                Button { choose() } label: { Label("Open", systemImage: "folder") }
                    .help("Open a JSON, YAML, TOML or CSV file")
                Button { data.paste() } label: { Label("Paste", systemImage: "doc.on.clipboard") }
                    .help("Paste copied text, or a file copied in Finder")
                Button("Copy Result") { data.copy() }
                    .disabled(data.output.isEmpty)
                Button("Save…") { data.save() }
                    .buttonStyle(.borderedProminent)
                    .disabled(data.output.isEmpty)
            }
        }
        .onChange(of: key) { data.changed() }
    }

    private var key: String {
        [data.text, data.from, data.to, data.indent, "\(data.minify)", "\(data.sortKeys)", "\(data.lenient)"]
            .joined(separator: "|")
    }

    private var controls: some View {
        HStack(spacing: 16) {
            Picker("Read as", selection: $data.from) {
                Text("Tell from the text").tag("")
                ForEach(DataModel.languages, id: \.self) { Text($0.uppercased()).tag($0) }
            }
            .fixedSize()
            Picker("Write as", selection: $data.to) {
                Text("Same language").tag("").disabled(data.reason("keep") != nil)
                ForEach(DataModel.languages, id: \.self) { Text($0.uppercased()).tag($0) }
            }
            .fixedSize()
            Picker("Indent", selection: $data.indent) {
                Text("2 spaces").tag("2")
                Text("4 spaces").tag("4")
                Text("Tab").tag("tab").disabled(data.reason("tab") != nil)
            }
            .fixedSize()
            .why(data.reason("indent"))
            Toggle("One line", isOn: $data.minify)
                .why(data.reason("minify"))
            Toggle("Sort keys", isOn: $data.sortKeys)
            Toggle("Lenient", isOn: $data.lenient)
                .help("Read JSON5 and JSONC (comments, trailing commas, single quotes), and drop YAML tags conv can't keep")
            Spacer()
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }

    private var input: some View {
        VStack(alignment: .leading, spacing: 0) {
            paneHeader(data.source.map { $0.lastPathComponent } ?? "Input",
                       language: data.inputLanguage)
            ZStack(alignment: .topLeading) {
                TextEditor(text: $data.text)
                    .font(.system(.body, design: .monospaced))
                    .scrollContentBackground(.hidden)
                    .padding(6)
                if data.text.isEmpty {
                    Text("Paste, type or drop JSON, YAML or TOML")
                        .font(.system(.body, design: .monospaced))
                        .foregroundStyle(.tertiary)
                        .padding(.horizontal, 12)
                        .padding(.vertical, 6)
                        .allowsHitTesting(false)
                }
            }
            if let p = data.problem { problemBox(p) }
        }
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
            data.load(first)
            return true
        } isTargeted: { dropHover = $0 }
    }

    private var result: some View {
        VStack(alignment: .leading, spacing: 0) {
            paneHeader("Result", language: data.output.isEmpty ? nil : data.outLanguage)
            ScrollView([.vertical, .horizontal]) {
                Text(data.output)
                    .font(.system(.body, design: .monospaced))
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .topLeading)
                    .padding(11)
            }
            .background(.background.secondary)
            VStack(alignment: .leading, spacing: 4) {
                ForEach(data.notes, id: \.self) { Text($0).font(.caption).foregroundStyle(.orange) }
                if let saved = data.saved {
                    Button("Saved \(saved.lastPathComponent). Show in Finder") {
                        NSWorkspace.shared.activateFileViewerSelecting([saved])
                    }
                    .buttonStyle(.link)
                }
                Text(data.command)
                    .font(.system(.caption, design: .monospaced))
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
            }
            .padding(10)
        }
    }

    private func paneHeader(_ title: String, language: String?) -> some View {
        HStack {
            Text(title).font(.headline).lineLimit(1).truncationMode(.middle)
            if let language {
                Text(language.uppercased())
                    .font(.caption.weight(.medium))
                    .padding(.horizontal, 6).padding(.vertical, 2)
                    .background(.tint.opacity(0.15), in: Capsule())
            }
            Spacer()
            if data.working { ProgressView().controlSize(.small) }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }

    private func problemBox(_ p: DataModel.Problem) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Label(p.message, systemImage: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
            if let snippet = p.snippet {
                let caret = String(repeating: " ", count: max(p.caret ?? 0, 0)) + "^"
                Text(snippet + "\n" + caret)
                    .font(.system(.callout, design: .monospaced))
                    .textSelection(.enabled)
            }
            if let hint = p.hint {
                Text(hint).font(.callout).foregroundStyle(.secondary)
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.orange.opacity(0.08))
    }

    private func choose() {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = ["json", "yaml", "yml", "toml", "csv"].compactMap { UTType(filenameExtension: $0) }
        if panel.runModal() == .OK, let url = panel.url { data.load(url) }
    }
}

import UniformTypeIdentifiers
