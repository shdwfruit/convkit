import AppKit
import Observation
import SwiftUI
import UniformTypeIdentifiers

/// What one run of a small tool sends conv.
struct QuickRun {
    var args: [String]
    /// Piped in, for a `-` argument.
    var stdin: String? = nil
    var env: [String: String] = [:]
    /// Where conv runs, for a command that writes into the folder it's in.
    var folder: URL? = nil
}

/// What a JSON-speaking tool's answer means, when its plain text wouldn't do.
struct QuickParsed {
    var text: String
    /// A file conv wrote into the app's temp folder, for Save….
    var file: URL? = nil
}

/// A small tool that takes text or a file and prints its answer: word count, case,
/// base64, checksums, timestamps and the rest. conv's own printed output is shown as
/// it is, so the app adds no rules of its own.
struct QuickSpec {
    enum Input { case none, text, optionalText, twoTexts, file, textOrFile }

    var input: Input
    var placeholder = ""
    var placeholder2 = ""
    /// For file input: conv kinds taken ("image"); empty takes anything.
    var fileKinds: Set<String> = []
    var options: [QuickOption] = []
    var build: (QuickModel) -> QuickRun?
    var parse: ((QuickModel, Conv.Output) -> QuickParsed?)? = nil
    /// A button that runs it again, for tools that make something new each time.
    var again: ((QuickModel) -> String?)? = nil
    var empty = "The result shows here."
    /// What an empty answer means (two texts that are the same).
    var blankResult: String? = nil
    var diff = false
    var monoInput = false
}

struct QuickOption: Identifiable {
    enum Kind {
        case choice([(String, String)])
        case toggle
        case field(String)
        case stepper(ClosedRange<Int>)
        case secret(String)
    }
    let key: String
    let label: String
    let kind: Kind
    var initial = ""
    var help: String? = nil
    /// Why it's greyed out for what's in the tool now.
    var why: ((QuickModel) -> String?)? = nil
    var id: String { key }
}

@Observable
final class QuickModel {
    let tool: ToolID
    let spec: QuickSpec
    var conv: URL?
    var text = ""
    var text2 = ""
    private(set) var file: URL?
    /// Text or File, for tools that take either.
    var useFile = false
    var values: [String: String]

    private(set) var output = ""
    private(set) var notes: [String] = []
    private(set) var problem: String?
    private(set) var savedFile: URL?
    private(set) var running = false
    private(set) var ran = false
    private var task: Task<Void, Never>?

    /// The app's own scratch folder, for what a tool writes before it's saved.
    let scratch = FileManager.default.temporaryDirectory.appendingPathComponent("convkit-quick", isDirectory: true)

    init(tool: ToolID, spec: QuickSpec) {
        self.tool = tool
        self.spec = spec
        var v: [String: String] = [:]
        for o in spec.options { v[o.key] = o.initial }
        values = v
    }

    func v(_ key: String) -> String { values[key] ?? "" }
    func on(_ key: String) -> Bool { values[key] == "1" }

    var takesFile: Bool { spec.input == .file || (spec.input == .textOrFile && useFile) }

    @MainActor
    func load(_ url: URL) {
        file = url
        if spec.input == .textOrFile { useFile = true }
        changed()
    }

    @MainActor
    func clearFile() {
        file = nil
        changed()
    }

    /// Text from the clipboard, or a file copied in Finder for a file tool.
    @MainActor
    func paste() {
        let pb = NSPasteboard.general
        if spec.input == .file || spec.input == .textOrFile,
           let url = (pb.readObjects(forClasses: [NSURL.self], options: [.urlReadingFileURLsOnly: true]) as? [URL])?.first {
            load(url)
            return
        }
        if let s = pb.string(forType: .string), spec.input != .file, spec.input != .none {
            useFile = false
            text = s
            changed()
        }
    }

    func copy() {
        let s = output.trimmingCharacters(in: .newlines)
        guard !s.isEmpty else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(s, forType: .string)
    }

    @MainActor
    func saveFile() {
        guard let savedFile, let dest = askWhereToSave(name: savedFile.lastPathComponent, types: [savedFile.pathExtension])
        else { return }
        try? FileManager.default.removeItem(at: dest)
        try? FileManager.default.copyItem(at: savedFile, to: dest)
        NSWorkspace.shared.activateFileViewerSelecting([dest])
    }

    /// The command, with the text shown as piped in.
    var command: String {
        guard let run = spec.build(self) else { return "" }
        let shown = run.args.map { a in
            a.contains(where: { " '\"$`\\()[]&;|<>*?!#~\n".contains($0) })
                ? "'" + a.replacingOccurrences(of: "'", with: "'\\''") + "'" : a
        }
        return (run.stdin != nil ? "… | " : "") + "conv " + shown.joined(separator: " ")
    }

    func changed(now: Bool = false) {
        task?.cancel()
        savedFile = nil
        guard let conv, let run = spec.build(self) else {
            output = ""
            notes = []
            problem = nil
            ran = false
            return
        }
        task = Task { @MainActor in
            if !now { try? await Task.sleep(for: .milliseconds(250)) }
            guard !Task.isCancelled else { return }
            running = true
            defer { running = false }
            try? FileManager.default.createDirectory(at: scratch, withIntermediateDirectories: true)
            guard let out = try? await Conv.run(conv, run.args, in: run.folder, stdin: run.stdin.map { Data($0.utf8) }, env: run.env),
                  !Task.isCancelled else { return }
            ran = true
            if let parse = spec.parse, let parsed = parse(self, out) {
                output = parsed.text
                savedFile = parsed.file
                notes = []
                problem = nil
                return
            }
            let lines = out.stderr.split(separator: "\n").map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty }
            let errors = lines.filter { $0.hasPrefix("error:") }
            output = String(decoding: out.stdout, as: UTF8.self)
            if let err = out.decode(QuickError.self)?.error {
                problem = AppModel.friendly(err)
            } else if !errors.isEmpty {
                // clap's usage text follows its error; the first line says it.
                problem = String(errors[0].dropFirst(6)).trimmingCharacters(in: .whitespaces)
            } else if out.status != 0 && output.isEmpty && !lines.isEmpty {
                // A check that failed (a hash that doesn't match) says so on stderr.
                problem = lines.joined(separator: "\n")
                notes = []
                return
            } else {
                problem = nil
            }
            notes = errors.isEmpty ? lines.map { $0.hasPrefix("note") ? String($0.dropFirst(4)).trimmingCharacters(in: .whitespaces) : $0 } : []
        }
    }

    private struct QuickError: Decodable { var error: ConvError? }
}

// MARK: - The tools

extension ToolID {
    var quickSpec: QuickSpec? {
        switch self {
        case .count:
            QuickSpec(input: .text, placeholder: "Type or paste the text to count",
                      build: { m in m.text.isEmpty ? nil : QuickRun(args: ["count", "-"], stdin: m.text) })
        case .changeCase:
            QuickSpec(
                input: .text, placeholder: "Type or paste text. The code cases work a line at a time.",
                options: [QuickOption(key: "style", label: "Case", kind: .choice([
                    ("Title Case", "title"), ("Sentence case", "sentence"), ("UPPER", "upper"), ("lower", "lower"),
                    ("camelCase", "camel"), ("PascalCase", "pascal"), ("snake_case", "snake"),
                    ("kebab-case", "kebab"), ("CONSTANT_CASE", "constant"),
                ]), initial: "title")],
                build: { m in m.text.isEmpty ? nil : QuickRun(args: ["case", m.v("style"), "-"], stdin: m.text) })
        case .cleanLinks:
            QuickSpec(
                input: .text, placeholder: "Paste links, or a message with links in it",
                options: [QuickOption(key: "follow", label: "Ask short links where they go", kind: .toggle,
                                      help: "Goes online: one HEAD request to each server along the way, with no cookies, downloading nothing.")],
                build: { m in
                    m.text.isEmpty ? nil : QuickRun(args: ["link", "-f", "-"] + (m.on("follow") ? ["--follow"] : []), stdin: m.text)
                })
        case .findChars:
            QuickSpec(
                input: .text, placeholder: "Paste text to see every character in it",
                options: [QuickOption(key: "suspicious", label: "Only the flagged ones", kind: .toggle)],
                build: { m in
                    m.text.isEmpty ? nil : QuickRun(args: ["chars"] + (m.on("suspicious") ? ["--suspicious"] : []) + ["-"], stdin: m.text)
                })
        case .compareText:
            QuickSpec(
                input: .twoTexts, placeholder: "The old text", placeholder2: "The new text",
                options: [
                    QuickOption(key: "context", label: "Lines around each change", kind: .stepper(0...20), initial: "3"),
                    QuickOption(key: "unified", label: "Plain unified diff", kind: .toggle,
                                help: "Without the changed words marked, for patch and git apply."),
                    QuickOption(key: "eol", label: "Count line endings", kind: .toggle,
                                help: "A CRLF line and an LF one count as different."),
                ],
                build: { m in
                    guard !m.text.isEmpty || !m.text2.isEmpty else { return nil }
                    var a = ["diff", "--text", "-U", m.v("context")]
                    if m.on("unified") { a.append("--unified") }
                    if m.on("eol") { a.append("--eol") }
                    return QuickRun(args: a + ["--", m.text, m.text2])
                },
                diff: true, monoInput: true)
        case .lorem:
            QuickSpec(
                input: .none,
                options: [
                    QuickOption(key: "n", label: "How many", kind: .stepper(1...50), initial: "2"),
                    QuickOption(key: "unit", label: "Of", kind: .choice([("Paragraphs", "-p"), ("Sentences", "-s"), ("Words", "-w")]),
                                initial: "-p"),
                    QuickOption(key: "html", label: "HTML", kind: .toggle, help: "Each paragraph in a <p>.",
                                why: { $0.on("markdown") ? "HTML or Markdown, not both." : nil }),
                    QuickOption(key: "markdown", label: "Markdown", kind: .toggle, help: "Lines wrapped at 80 columns.",
                                why: { $0.on("html") ? "HTML or Markdown, not both." : nil }),
                    QuickOption(key: "random", label: "Don't start with Lorem ipsum", kind: .toggle),
                ],
                build: { m in
                    var a = ["lorem", m.v("unit"), m.v("n")]
                    if m.on("html") { a.append("--html") }
                    if m.on("markdown") && !m.on("html") { a.append("--markdown") }
                    if m.on("random") { a.append("--random") }
                    return QuickRun(args: a)
                },
                again: { $0.on("random") ? "Make New" : nil })
        case .base64:
            QuickSpec(
                input: .textOrFile, placeholder: "Text to encode, or base64 or a data: URI to decode",
                options: [
                    QuickOption(key: "dir", label: "Direction", kind: .choice([("Encode", ""), ("Decode", "d")])),
                    QuickOption(key: "url", label: "URL-safe", kind: .toggle,
                                help: "- and _ instead of + and /, without = padding, as URLs and JWTs carry it.",
                                why: { $0.v("dir") == "d" ? "Decoding reads either alphabet." : nil }),
                    QuickOption(key: "uri", label: "As a data: URI", kind: .toggle,
                                why: { $0.v("dir") == "d" ? "Only for encoding." : nil }),
                ],
                build: { m in
                    let source: [String]
                    var stdin: String?
                    if m.takesFile {
                        guard let f = m.file else { return nil }
                        source = ["-f", f.path]
                    } else {
                        guard !m.text.isEmpty else { return nil }
                        source = ["-"]
                        stdin = m.text
                    }
                    if m.v("dir") == "d" {
                        // Decoded text comes back in the JSON; anything else is written into the
                        // folder conv runs in, the scratch folder, until Save….
                        return QuickRun(args: ["base64", "-d", "--json", "-y"] + source, stdin: stdin, folder: m.scratch)
                    }
                    var a = ["base64"]
                    if m.on("url") { a.append("--url") }
                    if m.on("uri") { a.append("--data-uri") }
                    return QuickRun(args: a + source, stdin: stdin)
                },
                parse: { m, out in
                    guard m.v("dir") == "d" else { return nil }
                    struct R: Decodable {
                        struct Item: Decodable { var text: String?; var output: String?; var bytes: Int64?; var mime: String? }
                        var ok: Bool
                        var results: [Item]?
                    }
                    guard let r = out.decode(R.self), r.ok, let item = r.results?.first else { return nil }
                    if let t = item.text { return QuickParsed(text: t) }
                    guard let path = item.output else { return nil }
                    let url = URL(fileURLWithPath: path, relativeTo: m.scratch).standardizedFileURL
                    let kind = item.mime ?? url.pathExtension.uppercased()
                    return QuickParsed(text: "Not text: \(kind), \(bytes(item.bytes ?? 0)). Save it to keep it.", file: url)
                },
                monoInput: true)
        case .hash:
            QuickSpec(
                input: .textOrFile, placeholder: "Text to hash, exactly as typed",
                options: [
                    QuickOption(key: "algo", label: "Hash", kind: .choice([
                        ("SHA-256", ""), ("SHA-512", "--sha512"), ("SHA-384", "--sha384"), ("SHA-1", "--sha1"),
                        ("MD5", "--md5"), ("All five", "--all"),
                    ]), why: { $0.v("check").isEmpty ? nil : "The expected hash's length says which it is." }),
                    QuickOption(key: "check", label: "Expected", kind: .field("Paste a hash to check against")),
                ],
                build: { m in
                    var a = ["hash"]
                    let check = m.v("check").trimmingCharacters(in: .whitespaces)
                    if check.isEmpty { if !m.v("algo").isEmpty { a.append(m.v("algo")) } } else { a += ["--check", check] }
                    if m.takesFile {
                        guard let f = m.file else { return nil }
                        return QuickRun(args: a + [f.path])
                    }
                    guard !m.text.isEmpty else { return nil }
                    return QuickRun(args: a + ["-"], stdin: m.text)
                },
                monoInput: true)
        case .jwt:
            QuickSpec(
                input: .text, placeholder: "Paste a token. It's read here and sent nowhere.",
                options: [QuickOption(key: "secret", label: "Secret", kind: .secret("To check an HMAC signature"),
                                      help: "Checks HS256, HS384 and HS512 signatures. The secret goes to conv privately.")],
                build: { m in
                    let token = m.text.trimmingCharacters(in: .whitespacesAndNewlines)
                    guard !token.isEmpty else { return nil }
                    if m.v("secret").isEmpty { return QuickRun(args: ["jwt", "-"], stdin: token) }
                    return QuickRun(args: ["jwt", "--secret-env", "CONVKIT_JWT_SECRET", "-"], stdin: token,
                                    env: ["CONVKIT_JWT_SECRET": m.v("secret")])
                },
                monoInput: true)
        case .timestamp:
            QuickSpec(
                input: .optionalText, placeholder: "1700000000, 2024-02-29 14:30, 29 Feb 2024 or 3 days ago. Empty for now.",
                options: [
                    QuickOption(key: "tz", label: "Also in", kind: .field("Asia/Tokyo")),
                    QuickOption(key: "unit", label: "A number is in", kind: .choice([
                        ("Its size says", ""), ("Seconds", "s"), ("Milliseconds", "ms"), ("Microseconds", "us"),
                        ("Nanoseconds", "ns"),
                    ]), why: { m in
                        let t = m.text.trimmingCharacters(in: .whitespacesAndNewlines)
                        return !t.isEmpty && Int(t) != nil ? nil : "Only for a number."
                    }),
                ],
                build: { m in
                    var a = ["time"]
                    let tz = m.v("tz").trimmingCharacters(in: .whitespaces)
                    if !tz.isEmpty { a += ["--tz", tz] }
                    let t = m.text.trimmingCharacters(in: .whitespacesAndNewlines)
                    if !m.v("unit").isEmpty && Int(t) != nil { a += ["--unit", m.v("unit")] }
                    return QuickRun(args: t.isEmpty ? a : a + ["--", t])
                },
                again: { $0.text.isEmpty ? "Now" : nil }, monoInput: true)
        case .uuid:
            QuickSpec(
                input: .optionalText, placeholder: "Paste a UUID to see what it is, or leave this empty to make some",
                options: [
                    QuickOption(key: "v7", label: "Kind", kind: .choice([("Random (v4)", ""), ("Time-ordered (v7)", "1")]),
                                why: { $0.text.isEmpty ? nil : "Reading a UUID, not making one." }),
                    QuickOption(key: "n", label: "How many", kind: .stepper(1...50), initial: "5",
                                why: { $0.text.isEmpty ? nil : "Reading a UUID, not making one." }),
                ],
                build: { m in
                    let t = m.text.trimmingCharacters(in: .whitespacesAndNewlines)
                    if !t.isEmpty { return QuickRun(args: ["uuid", "--", t]) }
                    return QuickRun(args: ["uuid", "-n", m.v("n")] + (m.on("v7") ? ["--v7"] : []))
                },
                again: { $0.text.isEmpty ? "Make More" : nil }, monoInput: true)
        case .info:
            QuickSpec(input: .file, build: { m in m.file.map { QuickRun(args: ["info", $0.path]) } },
                      empty: "Drop any file: a video, a photo, a PDF.")
        case .readQR:
            QuickSpec(input: .file, fileKinds: ["image"],
                      build: { m in m.file.map { QuickRun(args: ["qr", "--read", $0.path]) } },
                      empty: "Drop a screenshot or a photo with a QR code in it. Links are shown, never opened.")
        default:
            nil
        }
    }
}

// MARK: - View

struct QuickView: View {
    @Bindable var quick: QuickModel
    @Environment(AppModel.self) private var model

    var body: some View {
        VSplitView {
            if quick.spec.input != .none {
                inputArea
                    .frame(minHeight: quick.spec.input == .file ? 120 : 140, idealHeight: 220)
            }
            VStack(spacing: 0) {
                if !quick.spec.options.isEmpty {
                    optionsBar
                    Divider()
                }
                resultArea
            }
            .frame(minHeight: 200)
        }
        .navigationTitle(quick.tool.title)
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                if quick.takesFile || quick.spec.input == .file {
                    Button { chooseFile() } label: { Label("Open", systemImage: "folder") }
                }
                if quick.spec.input != .none {
                    Button { quick.paste() } label: { Label("Paste", systemImage: "doc.on.clipboard") }
                        .help(quick.spec.input == .file ? "Use a file copied in Finder" : "Paste the clipboard's text")
                }
                if let again = quick.spec.again?(quick) {
                    Button(again) { quick.changed(now: true) }
                }
                if quick.savedFile != nil {
                    Button("Save…") { quick.saveFile() }.buttonStyle(.borderedProminent)
                } else {
                    Button("Copy") { quick.copy() }
                        .buttonStyle(.borderedProminent)
                        .disabled(quick.output.isEmpty)
                        .keyboardShortcut("c", modifiers: [.command, .shift])
                        .help("Copy the result (⇧⌘C)")
                }
            }
        }
        .onAppear { if !quick.ran { quick.changed(now: true) } }
        .onChange(of: quick.text) { quick.changed() }
        .onChange(of: quick.text2) { quick.changed() }
        .onChange(of: quick.values) { quick.changed() }
        .onChange(of: quick.useFile) { quick.changed() }
    }

    // MARK: Input

    @ViewBuilder
    private var inputArea: some View {
        switch quick.spec.input {
        case .none:
            EmptyView()
        case .text, .optionalText:
            editor($quick.text, placeholder: quick.spec.placeholder)
        case .twoTexts:
            HSplitView {
                VStack(alignment: .leading, spacing: 4) {
                    Text("Old").font(.caption).foregroundStyle(.secondary).padding(.leading, 12).padding(.top, 8)
                    editor($quick.text, placeholder: quick.spec.placeholder)
                }
                VStack(alignment: .leading, spacing: 4) {
                    Text("New").font(.caption).foregroundStyle(.secondary).padding(.leading, 12).padding(.top, 8)
                    editor($quick.text2, placeholder: quick.spec.placeholder2)
                }
            }
        case .file:
            fileDrop
        case .textOrFile:
            VStack(spacing: 0) {
                Picker("Input", selection: $quick.useFile) {
                    Text("Text").tag(false)
                    Text("File").tag(true)
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .fixedSize()
                .padding(8)
                if quick.useFile { fileDrop } else { editor($quick.text, placeholder: quick.spec.placeholder) }
            }
        }
    }

    private func editor(_ text: Binding<String>, placeholder: String) -> some View {
        TextEditor(text: text)
            .font(quick.spec.monoInput ? .system(.body, design: .monospaced) : .body)
            .scrollContentBackground(.hidden)
            .padding(8)
            .overlay(alignment: .topLeading) {
                if text.wrappedValue.isEmpty {
                    Text(placeholder)
                        .foregroundStyle(.tertiary)
                        .padding(.horizontal, 13)
                        .padding(.vertical, 8)
                        .allowsHitTesting(false)
                }
            }
    }

    private var fileDrop: some View {
        HStack(spacing: 14) {
            if let file = quick.file {
                Image(nsImage: NSWorkspace.shared.icon(forFile: file.path))
                    .resizable().frame(width: 40, height: 40)
                VStack(alignment: .leading, spacing: 2) {
                    Text(file.lastPathComponent).font(.headline).lineLimit(1).truncationMode(.middle)
                    Text(file.deletingLastPathComponent().path).font(.caption).foregroundStyle(.secondary)
                        .lineLimit(1).truncationMode(.head)
                }
                Spacer()
                Button("Choose Another…") { chooseFile() }
            } else {
                ToolIcon(tool: quick.tool, size: 40)
                Text(quick.spec.empty).foregroundStyle(.secondary)
                Spacer()
                Button("Choose…") { chooseFile() }
            }
        }
        .padding(.horizontal, 20)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .contentShape(Rectangle())
        .dropDestination(for: URL.self) { urls, _ in
            guard let first = urls.first else { return false }
            quick.load(first)
            return true
        }
    }

    // MARK: Options

    private var optionsBar: some View {
        HStack(spacing: 18) {
            ForEach(quick.spec.options) { option in
                optionControl(option)
                    .disabled(option.why?(quick) != nil)
                    .help(option.why?(quick) ?? option.help ?? "")
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
        .background(.bar)
    }

    @ViewBuilder
    private func optionControl(_ option: QuickOption) -> some View {
        let binding = Binding(get: { quick.values[option.key] ?? "" }, set: { quick.values[option.key] = $0 })
        switch option.kind {
        case .choice(let choices):
            Picker(option.label, selection: binding) {
                ForEach(choices, id: \.1) { Text($0.0).tag($0.1) }
            }
            .fixedSize()
        case .toggle:
            Toggle(option.label, isOn: Binding(get: { binding.wrappedValue == "1" }, set: { binding.wrappedValue = $0 ? "1" : "" }))
        case .field(let prompt):
            HStack(spacing: 6) {
                Text(option.label)
                TextField(option.label, text: binding, prompt: Text(prompt))
                    .labelsHidden()
                    .textFieldStyle(.roundedBorder)
                    .frame(minWidth: 160, maxWidth: 420)
            }
        case .secret(let prompt):
            HStack(spacing: 6) {
                Text(option.label)
                SecureField(option.label, text: binding, prompt: Text(prompt))
                    .labelsHidden()
                    .textFieldStyle(.roundedBorder)
                    .frame(minWidth: 160, maxWidth: 300)
            }
        case .stepper(let range):
            let n = Int(binding.wrappedValue) ?? range.lowerBound
            Stepper("\(option.label): \(n)", value: Binding(get: { n }, set: { binding.wrappedValue = String($0) }), in: range)
                .fixedSize()
        }
    }

    // MARK: Result

    private var resultArea: some View {
        VStack(alignment: .leading, spacing: 0) {
            ScrollView {
                VStack(alignment: .leading, spacing: 10) {
                    if let problem = quick.problem {
                        Label(problem, systemImage: "exclamationmark.triangle")
                            .foregroundStyle(.orange)
                            .textSelection(.enabled)
                    }
                    if !quick.output.isEmpty {
                        Group {
                            if quick.spec.diff { Text(diffText(quick.output)) } else { Text(quick.output) }
                        }
                        .font(.system(.body, design: .monospaced))
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    } else if quick.ran && quick.problem == nil, let blank = quick.spec.blankResult {
                        Label(blank, systemImage: "checkmark.circle").foregroundStyle(.green)
                    } else if !quick.ran && quick.problem == nil {
                        Text(quick.spec.input == .file ? "" : quick.spec.empty).foregroundStyle(.tertiary)
                    }
                    ForEach(quick.notes, id: \.self) { note in
                        Text(note).font(.callout).foregroundStyle(.secondary).textSelection(.enabled)
                    }
                }
                .padding(14)
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            Divider()
            HStack(spacing: 8) {
                if quick.running { ProgressView().controlSize(.small) }
                Text(quick.command.isEmpty ? " " : quick.command)
                    .font(.system(.caption, design: .monospaced))
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .textSelection(.enabled)
                Spacer()
                if !model.convVersion.isEmpty {
                    Text(model.convVersion).font(.caption2).foregroundStyle(.tertiary)
                }
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 6)
        }
    }

    /// conv marks changed words [-like this-] and {+like this+} when it isn't writing
    /// to a terminal; this colours them and the changed lines.
    private func diffText(_ s: String) -> AttributedString {
        var out = AttributedString()
        for line in s.split(separator: "\n", omittingEmptySubsequences: false) {
            let l = String(line)
            var base = AttributeContainer()
            if l.hasPrefix("@@") {
                base.foregroundColor = .secondary
            } else if l.hasPrefix("-") && !l.hasPrefix("---") {
                base.backgroundColor = Color.red.opacity(0.12)
            } else if l.hasPrefix("+") && !l.hasPrefix("+++") {
                base.backgroundColor = Color.green.opacity(0.12)
            }
            var rest = Substring(l)
            while !rest.isEmpty {
                let del = rest.range(of: "[-"), ins = rest.range(of: "{+")
                guard let open = [del, ins].compactMap({ $0 }).min(by: { $0.lowerBound < $1.lowerBound }) else {
                    out += AttributedString(String(rest), attributes: base)
                    break
                }
                let isDel = open == del
                let close = rest[open.upperBound...].range(of: isDel ? "-]" : "+}")
                guard let close else {
                    out += AttributedString(String(rest), attributes: base)
                    break
                }
                out += AttributedString(String(rest[..<open.lowerBound]), attributes: base)
                var mark = base
                if isDel {
                    mark.foregroundColor = .red
                    mark.strikethroughStyle = .single
                } else {
                    mark.foregroundColor = .green
                    mark.inlinePresentationIntent = .stronglyEmphasized
                }
                out += AttributedString(String(rest[open.upperBound..<close.lowerBound]), attributes: mark)
                rest = rest[close.upperBound...]
            }
            out += AttributedString("\n", attributes: base)
        }
        return out
    }

    private func chooseFile() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = quick.tool == .info
        if quick.spec.fileKinds.contains("image") { panel.allowedContentTypes = [.image] }
        if panel.runModal() == .OK, let url = panel.url { quick.load(url) }
    }
}
