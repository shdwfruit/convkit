import AppKit
import Foundation
import Observation

/// One dropped file and what conv says will happen to it.
@Observable
final class FileItem: Identifiable {
    enum Status: Equatable {
        case idle
        case running
        case done(output: URL, bytes: Int64?)
        case failed(String)
        case exists(String)        // output_exists: offer Replace
        case needsConfirm(String)  // confirmation_required: offer Convert anyway
    }

    let id = UUID()
    let url: URL
    let size: Int64?
    var ext: String { url.pathExtension.lowercased() }

    // From the latest dry run.
    var plannedOutput: URL?
    var problem: String?
    var warnings: [String] = []

    var status: Status = .idle
    var notes: [String] = []

    init(url: URL) {
        self.url = url
        self.size = (try? url.resourceValues(forKeys: [.fileSizeKey]).fileSize).map(Int64.init)
    }
}

/// Flags conv applies when a lone file keeps its format.
private let sameFormatTuning: Set<String> = [
    "--max-size", "--strip-metadata", "--start", "--end", "--duration", "--speed", "--shift-pitch",
]

/// The app's state: which tool is showing, the conv in use, and Convert's own state.
@Observable
final class AppModel {
    var tool: Tool = .convert
    let qr = QRModel()
    let ascii = AsciiModel()
    let trim = TrimModel()
    let scan = ScanModel()
    let backends = BackendsModel()
    let formats = FormatsModel()
    let changelog = ChangelogModel()
    /// Subcommands the conv in use offers, from its --help.
    private(set) var subcommands: Set<String> = []

    var availableTools: [Tool] {
        Tool.allCases.filter { $0.subcommand.map(subcommands.contains) ?? true }
    }

    var files: [FileItem] = []

    // Options. Empty string means "no flag".
    var format = "same"
    var maxSize = ""
    var resize = ""
    var upscale = false
    var colors = ""
    var fps = ""
    var crf = ""
    var duration = ""
    var quality: Double = 92
    var useQuality = false
    var speed = ""
    var shiftPitch = false
    var start = ""
    var end = ""
    var strip = false
    var outputFolder: URL?

    // Derived from `conv capabilities`.
    private(set) var targets: [String] = []
    private(set) var tuning: Set<String> = sameFormatTuning
    private(set) var targetNotes: [String] = []
    private var caps: [String: CapabilitiesResponse] = [:]

    private(set) var convURL: URL?
    private(set) var convVersion = ""
    private(set) var isRunning = false
    private(set) var setupProblem: String?
    var convOverride: String = UserDefaults.standard.string(forKey: "convPath") ?? "" {
        didSet {
            UserDefaults.standard.set(convOverride, forKey: "convPath")
            Task { await connect() }
        }
    }

    private var previewTask: Task<Void, Never>?
    private var runTask: Task<Void, Never>?

    // MARK: Setup

    @MainActor
    func connect() async {
        caps = [:]
        guard let url = Conv.locate(override: convOverride) else {
            convURL = nil
            setupProblem = "Can't find conv. Install it, or set its path in Settings."
            return
        }
        convURL = url
        qr.conv = url
        ascii.conv = url
        trim.conv = url
        scan.conv = url
        backends.conv = url
        formats.conv = url
        changelog.conv = url
        setupProblem = nil
        if let out = try? await Conv.run(url, ["--version"]) {
            convVersion = String(decoding: out.stdout, as: UTF8.self)
                .trimmingCharacters(in: .whitespacesAndNewlines)
        }
        if let out = try? await Conv.run(url, ["--help"]) {
            subcommands = Self.parseSubcommands(String(decoding: out.stdout, as: UTF8.self))
        }
        if !availableTools.contains(tool) { tool = .convert }
        await refreshTargets()
    }

    /// The names under "Commands:" in conv's help.
    static func parseSubcommands(_ help: String) -> Set<String> {
        var names: Set<String> = []
        var inCommands = false
        for line in help.split(separator: "\n", omittingEmptySubsequences: false) {
            if line.hasPrefix("Commands:") { inCommands = true; continue }
            guard inCommands else { continue }
            guard line.hasPrefix("  ") else { break }
            if let name = line.split(separator: " ").first { names.insert(String(name)) }
        }
        return names
    }

    /// A lone file from Finder or the Dock goes to ASCII Art or Trim when that tool is showing;
    /// anything else goes to Convert.
    @MainActor
    func open(_ urls: [URL]) {
        if tool == .ascii, urls.count == 1 {
            ascii.input = urls[0]
        } else if tool == .trim, urls.count == 1 {
            trim.load(urls[0])
        } else {
            tool = .convert
            add(urls)
        }
    }

    // MARK: Files

    @MainActor
    func add(_ urls: [URL]) {
        let fm = FileManager.default
        var expanded: [URL] = []
        for url in urls {
            var isDir: ObjCBool = false
            guard fm.fileExists(atPath: url.path, isDirectory: &isDir) else { continue }
            if isDir.boolValue {
                // One level down, visible files only.
                let children = (try? fm.contentsOfDirectory(
                    at: url, includingPropertiesForKeys: [.isRegularFileKey],
                    options: [.skipsHiddenFiles])) ?? []
                expanded += children
                    .filter { (try? $0.resourceValues(forKeys: [.isRegularFileKey]).isRegularFile) == true }
                    .sorted { $0.lastPathComponent.localizedStandardCompare($1.lastPathComponent) == .orderedAscending }
            } else {
                expanded.append(url)
            }
        }
        let known = Set(files.map { $0.url.standardizedFileURL })
        files += expanded
            .filter { !known.contains($0.standardizedFileURL) }
            .map(FileItem.init)
        Task { await refreshTargets() }
    }

    @MainActor
    func remove(_ item: FileItem) {
        files.removeAll { $0.id == item.id }
        Task { await refreshTargets() }
    }

    @MainActor
    func clear() {
        files = []
        Task { await refreshTargets() }
    }

    // MARK: Targets

    /// Targets every file can reach, and the flags that apply to the chosen one.
    @MainActor
    func refreshTargets() async {
        guard let conv = convURL else { return }
        let exts = Set(files.map(\.ext))
        for ext in exts where caps[ext] == nil {
            guard !ext.isEmpty,
                  let out = try? await Conv.run(conv, ["capabilities", ext, "--json"]),
                  let resp = out.decode(CapabilitiesResponse.self)
            else { continue }
            caps[ext] = resp
        }
        var common: [String]? = nil
        for ext in exts {
            let tos = caps[ext]?.targets?.map(\.to) ?? []
            common = common.map { c in c.filter(tos.contains) } ?? tos
        }
        targets = common ?? []
        if format != "same" && !targets.contains(format) { format = "same" }
        optionsChanged()
    }

    private func updateTuning() {
        guard format != "same" else {
            tuning = sameFormatTuning
            targetNotes = []
            return
        }
        var flags: Set<String>? = nil
        var notes: [String] = []
        for ext in Set(files.map(\.ext)) {
            guard let t = caps[ext]?.targets?.first(where: { $0.to == format }) else { continue }
            flags = flags.map { $0.intersection(t.tuning) } ?? Set(t.tuning)
            notes += t.notes ?? []
        }
        tuning = flags ?? []
        targetNotes = Array(Set(notes)).sorted()
    }

    func applies(_ flag: String) -> Bool { tuning.contains(flag) }

    /// Options the user set that the chosen target ignores, so the form can say so.
    var ignoredOptions: [String] {
        var out: [String] = []
        func check(_ isSet: Bool, _ flag: String, _ name: String) {
            if isSet && !applies(flag) { out.append(name) }
        }
        check(!maxSize.isEmpty, "--max-size", "Keep under")
        check(!resize.isEmpty, "--resize", "Resize")
        check(useQuality, "--quality", "Quality")
        check(!colors.isEmpty, "--colors", "Colors")
        check(!fps.isEmpty, "--fps", "Frame rate")
        check(!crf.isEmpty, "--crf", "Video quality")
        check(!duration.isEmpty, "--duration", "Keep")
        check(!speed.isEmpty, "--speed", "Speed")
        check(!start.isEmpty, "--start", "Start")
        check(!end.isEmpty, "--end", "End")
        check(strip, "--strip-metadata", "Remove metadata")
        return out
    }

    // MARK: Arguments

    /// conv's argv for one file, without --json/--dry-run/-y.
    func args(for item: FileItem) -> [String] {
        var a = [item.url.path]
        if format != "same" { a.append("." + format) }
        func add(_ flag: String, _ value: String) {
            let v = value.trimmingCharacters(in: .whitespaces)
            if !v.isEmpty && applies(flag) { a += [flag, v] }
        }
        add("--max-size", maxSize.replacingOccurrences(of: " ", with: ""))
        add("--resize", resize)
        if upscale && !resize.isEmpty && applies("--upscale") { a.append("--upscale") }
        add("--colors", colors)
        add("--fps", fps)
        add("--crf", crf)
        if useQuality && applies("--quality") { a += ["--quality", String(Int(quality))] }
        add("--speed", speed)
        if shiftPitch && !speed.isEmpty && applies("--shift-pitch") { a.append("--shift-pitch") }
        add("--start", start)
        add("--end", end)
        add("--duration", duration)
        if strip && applies("--strip-metadata") { a.append("--strip-metadata") }
        if let dir = outputFolder { a += ["-o", dir.path] }
        return a
    }

    /// The command for the first file, for the "Show command" disclosure.
    var commandPreview: String {
        guard let first = files.first else { return "" }
        let parts = ["conv"] + args(for: first).map { arg in
            arg.contains(where: { " '\"$`\\()[]&;|<>*?!#~".contains($0) })
                ? "'" + arg.replacingOccurrences(of: "'", with: "'\\''") + "'"
                : arg
        }
        return parts.joined(separator: " ")
    }

    // MARK: Preview

    /// Called on every option change: asks conv (dry run) what each file would become.
    func optionsChanged() {
        updateTuning()
        previewTask?.cancel()
        let items = files
        guard let conv = convURL, !items.isEmpty else { return }
        let argv = items.map { args(for: $0) }
        previewTask = Task { @MainActor in
            try? await Task.sleep(for: .milliseconds(200))
            guard !Task.isCancelled else { return }
            for (item, args) in zip(items, argv) {
                guard !Task.isCancelled else { return }
                if nothingToDo(args) {
                    if !item.isDone { item.status = .idle }
                    item.plannedOutput = nil
                    item.warnings = []
                    item.problem = Self.nothingToDoText
                    continue
                }
                guard let out = try? await Conv.run(conv, args + ["--dry-run", "--json", "--no-install"])
                else { continue }
                guard !Task.isCancelled else { return }
                apply(dryRun: out, to: item)
            }
        }
    }

    static let nothingToDoText = "Choose something to change, or pick another format."

    /// Same format and no flag: conv would refuse, so say what to do instead.
    private func nothingToDo(_ args: [String]) -> Bool {
        format == "same" && !args.dropFirst().contains { $0.hasPrefix("--") }
    }

    @MainActor
    private func apply(dryRun out: Conv.Output, to item: FileItem) {
        if case .done = item.status {} else { item.status = .idle }
        guard let resp = out.decode(DryRunResponse.self) else {
            item.plannedOutput = nil
            item.warnings = []
            item.problem = Conv.stderrMessage(out.stderr)
            return
        }
        let entry = resp.plans?.first
        if let err = entry?.error ?? resp.error {
            item.plannedOutput = nil
            item.warnings = []
            item.problem = Self.friendly(err)
            return
        }
        item.problem = nil
        item.plannedOutput = entry?.plan?.output.map(URL.init(fileURLWithPath:))
        var warnings = entry?.plan?.warnings ?? []
        if let w = entry?.plan?.sizing?.warning {
            warnings.append(w + (entry?.plan?.sizing?.suggested.map { " Try \($0)." } ?? ""))
        }
        item.warnings = warnings
    }

    /// conv's messages are written for a terminal; soften the CLI bits.
    static func friendly(_ err: ConvError) -> String {
        switch err.code {
        case "unsupported_pair":
            return "conv can't make that from this file."
        case "backend_missing":
            return err.message + " Run `conv doctor` in Terminal for the install command."
        default:
            return err.message
                .replacingOccurrences(of: "; run `conv capabilities` to list supported pairs", with: "")
        }
    }

    // MARK: Run

    var readyCount: Int {
        files.filter { $0.problem == nil && $0.plannedOutput != nil && !$0.isDone }.count
    }

    @MainActor
    func convertAll() {
        guard let conv = convURL else { return }
        let items = files.filter { $0.problem == nil && !$0.isDone }
        isRunning = true
        runTask = Task { @MainActor in
            for item in items {
                if Task.isCancelled { break }
                await run(item, conv: conv, extra: [])
            }
            isRunning = false
        }
    }

    /// Re-run one file after the user said yes to Replace or Convert anyway.
    @MainActor
    func retry(_ item: FileItem, extra: [String]) {
        guard let conv = convURL else { return }
        isRunning = true
        runTask = Task { @MainActor in
            await run(item, conv: conv, extra: extra)
            isRunning = false
        }
    }

    @MainActor
    func cancel() {
        runTask?.cancel()
    }

    @MainActor
    private func run(_ item: FileItem, conv: URL, extra: [String]) async {
        item.status = .running
        item.notes = []
        // --yes answers conv's prompts, so it only goes in when the user chose Convert anyway.
        let guardFlags = extra.contains("--yes") ? [] : ["--no-install"]
        do {
            let out = try await Conv.run(conv, args(for: item) + ["--json"] + guardFlags + extra)
            if Task.isCancelled {
                item.status = .failed("Stopped.")
                return
            }
            guard let resp = out.decode(RunResponse.self) else {
                item.status = .failed(Conv.stderrMessage(out.stderr))
                return
            }
            let r = resp.results?.first
            if let r, r.ok, let output = r.output {
                item.status = .done(output: URL(fileURLWithPath: output), bytes: r.bytes)
                item.notes = (r.warnings ?? []) + (r.notes ?? [])
                return
            }
            guard let err = r?.error ?? resp.error else {
                item.status = .failed("conv didn't say what went wrong.")
                return
            }
            switch err.code {
            case "output_exists":
                item.status = .exists("\(item.plannedOutput?.lastPathComponent ?? "The new file") already exists.")
            case "confirmation_required":
                item.status = .needsConfirm(item.warnings.first ?? err.message)
            default:
                item.status = .failed(Self.friendly(err))
            }
        } catch {
            item.status = .failed(error.localizedDescription)
        }
    }
}

extension FileItem {
    var isDone: Bool {
        if case .done = status { return true }
        return false
    }
}
