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
    /// A picture pasted from the clipboard, saved in the app's own folder.
    var pasted: Bool { Clipboard.isPasted(url) }
    var displayName: String { pasted ? "Pasted picture (\(ext.uppercased()))" : url.lastPathComponent }

    /// Set when the tool doesn't take this kind of file; it's then left out.
    var misfit: String?

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

    var isDone: Bool {
        if case .done = status { return true }
        return false
    }
}

/// Every format conv knows, from `conv capabilities`, loaded once per conv.
@Observable
final class CapsStore {
    var conv: URL?
    private(set) var byExt: [String: CapabilitiesResponse] = [:]

    private struct All: Decodable {
        struct Pair: Decodable { var from: String; var to: String }
        var pairs: [Pair]?
    }

    @MainActor
    func load() async {
        byExt = [:]
        guard let conv,
              let out = try? await Conv.run(conv, ["capabilities", "--json"]),
              let all = out.decode(All.self) else { return }
        let names = Set((all.pairs ?? []).flatMap { [$0.from, $0.to] })
        var found: [String: CapabilitiesResponse] = [:]
        await withTaskGroup(of: (String, CapabilitiesResponse?).self) { group in
            for name in names {
                group.addTask {
                    let out = try? await Conv.run(conv, ["capabilities", name, "--json"])
                    return (name, out?.decode(CapabilitiesResponse.self))
                }
            }
            for await (name, resp) in group { if let resp { found[name] = resp } }
        }
        byExt = found
    }

    /// Looks up extensions conv knows by another name (jpeg, yml, tif).
    @MainActor
    func ensure(_ exts: Set<String>) async {
        guard let conv else { return }
        for ext in exts where !ext.isEmpty && byExt[ext] == nil {
            guard let out = try? await Conv.run(conv, ["capabilities", ext, "--json"]),
                  let resp = out.decode(CapabilitiesResponse.self), resp.ok else { continue }
            byExt[ext] = resp
        }
    }

    func kind(_ ext: String) -> String? { byExt[ext]?.kind }
    func name(_ ext: String) -> String { byExt[ext]?.format ?? ext }
    func targets(_ ext: String) -> [CapabilitiesResponse.Target] { byExt[ext]?.targets ?? [] }
    func reaches(_ ext: String, _ target: String) -> Bool { targets(ext).contains { $0.to == target } }

    /// Flags that apply when conv writes this format: what its sources of the same kind
    /// take on the way to it (any source when none is the same kind). A PNG gets
    /// resize, colors and rotate but not quality; a GIF gets the time flags, from video.
    func writeTuning(_ ext: String) -> Set<String> {
        guard let me = byExt[ext], let name = me.format else { return [] }
        let sources = me.sources ?? []
        let sameKind = sources.filter { byExt[$0]?.kind == me.kind }
        var flags: Set<String> = []
        for s in sameKind.isEmpty ? sources : sameKind {
            if let t = byExt[s]?.targets?.first(where: { $0.to == name }) { flags.formUnion(t.tuning) }
        }
        return flags
    }
}

/// Flags that make conv write a lone file in its own format (photo-50pct.jpg,
/// clip-rotated.mp4). Quality, colors, frame rate and CRF only refine one of these.
let keepFormatTriggers: Set<String> = [
    "--max-size", "--strip-metadata", "--start", "--end", "--duration", "--speed",
    "--resize", "--rotate",
]

/// One convert-style tool's files and settings. Every tool built on Convert has its
/// own, so Compress Video and Convert Image don't share files or knobs.
@Observable
final class ConvertJob {
    let tool: ToolID
    let preset: ConvertPreset
    let caps: CapsStore
    var conv: URL? { caps.conv }
    /// The everything tool, where a knob greys out until a file says it fits.
    var isGeneral: Bool { tool == .convert }

    var files: [FileItem] = []

    /// "same", a target extension, or "" when nothing fits.
    var format = "same"
    /// The user picked the format, so a new file doesn't reset it.
    private var formatChosen = false

    // Options. Empty string means "no flag".
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
    var rotate = ""
    /// Put the result on the clipboard (`--copy`).
    var copyResult = false
    var outputFolder: URL?

    private(set) var targets: [String] = []
    /// Flags conv takes for this set of files and format; nil before there are files.
    private(set) var tuning: Set<String>?
    private(set) var targetNotes: [String] = []

    private(set) var isRunning = false
    /// Shown after a run: what went on the clipboard, or why not.
    private(set) var clipboardNote: String?
    /// Why the last Paste brought nothing in.
    var pasteProblem: String?

    private var previewTask: Task<Void, Never>?
    private var runTask: Task<Void, Never>?

    init(tool: ToolID, caps: CapsStore) {
        self.tool = tool
        self.preset = tool.preset ?? ConvertPreset(target: .pick(kinds: [], allowSame: true, preferred: []),
                                                   controls: Control.allCases)
        self.caps = caps
        strip = preset.alwaysStrip
        if let fixed = preset.fixedTarget { format = fixed }
    }

    var fitting: [FileItem] { files.filter { $0.misfit == nil } }

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

    /// Files copied in Finder, or a copied picture or screenshot.
    @MainActor
    func paste() async {
        guard let conv else { return }
        switch await Clipboard.take(conv: conv) {
        case .success(let urls):
            pasteProblem = nil
            add(urls)
        case .failure(let problem):
            pasteProblem = problem.message
        }
    }

    @MainActor
    func remove(_ item: FileItem) {
        files.removeAll { $0.id == item.id }
        Task { await refreshTargets() }
    }

    @MainActor
    func clear() {
        files = []
        clipboardNote = nil
        Task { await refreshTargets() }
    }

    // MARK: Targets

    /// Marks files the tool doesn't take, then works out the targets every other file
    /// reaches and the flags that apply to the chosen one.
    @MainActor
    func refreshTargets() async {
        await caps.ensure(Set(files.map(\.ext)))
        for item in files {
            item.misfit = preset.takes(item.ext, kind: caps.kind(item.ext))
                ? nil
                : "\(tool.title) takes \(preset.noun). Convert Anything can work with this one."
        }
        var common: [String]? = nil
        for ext in Set(fitting.map(\.ext)) {
            let tos = caps.targets(ext).map(\.to).filter(targetFits)
            common = common.map { c in c.filter(tos.contains) } ?? tos
        }
        targets = common ?? []
        switch preset.target {
        case .fixed(let t):
            format = t
        case .pick(_, let allowSame, let preferred):
            let stillFits = format == "same" ? allowSame : targets.contains(format)
            if !(formatChosen && stillFits) {
                formatChosen = false
                format = allowSame ? "same" : (preferred.first(where: targets.contains) ?? targets.first ?? "")
            }
        }
        optionsChanged()
    }

    /// A target the tool offers: Extract Audio offers audio, Convert Image pictures.
    private func targetFits(_ to: String) -> Bool {
        guard case .pick(let kinds, _, _) = preset.target, !kinds.isEmpty else { return true }
        return caps.kind(to).map(kinds.contains) ?? false
    }

    var allowsSame: Bool {
        if case .pick(_, let allowSame, _) = preset.target { return allowSame }
        return false
    }

    /// The format picker's setter: the user's own choice sticks.
    @MainActor
    func choose(format f: String) {
        format = f
        formatChosen = true
    }

    private func updateTuning() {
        let exts = Set(fitting.map(\.ext))
        guard !exts.isEmpty else {
            tuning = nil
            targetNotes = []
            return
        }
        var flags: Set<String>? = nil
        var notes: [String] = []
        for ext in exts {
            let these: Set<String>
            if format == "same" {
                these = caps.writeTuning(ext)
            } else if let t = caps.targets(ext).first(where: { $0.to == format }) {
                these = Set(t.tuning)
                notes += t.notes ?? []
            } else {
                these = []
            }
            flags = flags.map { $0.intersection(these) } ?? these
        }
        tuning = flags ?? []
        targetNotes = Array(Set(notes)).sorted()
    }

    /// Whether conv takes this flag for these files. Before there are files, a focused
    /// tool's own knobs are on (they were chosen for its kind of file); the everything
    /// tool's wait for a file.
    func applies(_ flag: String) -> Bool {
        guard let tuning else { return !isGeneral }
        return tuning.contains(flag)
    }

    /// Why a knob is greyed out, or nil when it isn't.
    func reason(_ flag: String) -> String? {
        if applies(flag) {
            if flag == "--duration" && !end.isEmpty { return "Set an end or a length, not both." }
            if flag == "--end" && !duration.isEmpty { return "Set an end or a length, not both." }
            return nil
        }
        guard tuning != nil else { return "Add a file to see what fits it." }
        let names = Set(fitting.map { caps.name($0.ext).uppercased() }).sorted().joined(separator: ", ")
        if format == "same" { return "Doesn't apply to \(names) files." }
        return "Doesn't apply to \(names) → \(format.uppercased())."
    }

    func enabled(_ flag: String) -> Bool { reason(flag) == nil }

    /// Whether any of a control's flags applies, for greying a whole section.
    func applies(_ control: Control) -> Bool { control.flags.contains(where: applies) }

    /// Options the user set that conv would ignore for these files, so the form can say so.
    var ignoredOptions: [String] {
        guard tuning != nil else { return [] }
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
        check(!rotate.isEmpty, "--rotate", "Turn")
        return out
    }

    // MARK: Arguments

    /// conv's argv for one file, without --json/--dry-run/-y.
    func args(for item: FileItem) -> [String] {
        var a = [item.url.path]
        if format != "same" && !format.isEmpty { a.append("." + format) }
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
        if duration.isEmpty { add("--end", end) } else { add("--duration", duration) }
        if strip && applies("--strip-metadata") { a.append("--strip-metadata") }
        add("--rotate", rotate)
        if let dir = outputFolder {
            a += ["-o", dir.path]
        } else if item.pasted {
            a += ["-o", Clipboard.pastedOutputFolder.path]
        }
        return a
    }

    /// The command for the first file, for the "Command" disclosure.
    var commandPreview: String {
        guard let first = fitting.first else { return "" }
        let parts = ["conv"] + args(for: first).map { arg in
            arg.contains(where: { " '\"$`\\()[]&;|<>*?!#~".contains($0) })
                ? "'" + arg.replacingOccurrences(of: "'", with: "'\\''") + "'"
                : arg
        }
        return parts.joined(separator: " ")
    }

    /// Settings to carry into Convert Anything.
    @MainActor
    func copySettings(to other: ConvertJob) {
        other.maxSize = maxSize
        other.resize = resize
        other.upscale = upscale
        other.colors = colors
        other.fps = fps
        other.crf = crf
        other.duration = duration
        other.quality = quality
        other.useQuality = useQuality
        other.speed = speed
        other.shiftPitch = shiftPitch
        other.start = start
        other.end = end
        other.strip = strip
        other.rotate = rotate
        other.copyResult = copyResult
        other.outputFolder = outputFolder
        if format != "same" && !format.isEmpty { other.choose(format: format) }
        other.add(fitting.map(\.url))
    }

    // MARK: Preview

    /// Called on every option change: asks conv (dry run) what each file would become.
    func optionsChanged() {
        updateTuning()
        previewTask?.cancel()
        let items = fitting
        guard let conv, !items.isEmpty else { return }
        let argv = items.map { args(for: $0) }
        previewTask = Task { @MainActor in
            try? await Task.sleep(for: .milliseconds(200))
            guard !Task.isCancelled else { return }
            for (item, args) in zip(items, argv) {
                guard !Task.isCancelled else { return }
                if let why = nothingToDo(args) {
                    if !item.isDone { item.status = .idle }
                    item.plannedOutput = nil
                    item.warnings = []
                    item.problem = why
                    continue
                }
                guard let out = try? await Conv.run(conv, args + ["--dry-run", "--json", "--no-install"])
                else { continue }
                guard !Task.isCancelled else { return }
                apply(dryRun: out, to: item)
            }
        }
    }

    /// conv would refuse these, so say what to do instead.
    private func nothingToDo(_ args: [String]) -> String? {
        if format.isEmpty { return "conv can't make anything this tool offers from this file." }
        if format == "same" && !args.contains(where: keepFormatTriggers.contains) { return preset.needs }
        return nil
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
            item.problem = AppModel.friendly(err)
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

    // MARK: Run

    var readyCount: Int {
        fitting.filter { $0.problem == nil && $0.plannedOutput != nil && !$0.isDone }.count
    }

    @MainActor
    func runAll() {
        guard let conv else { return }
        let items = fitting.filter { $0.problem == nil && $0.plannedOutput != nil && !$0.isDone }
        isRunning = true
        clipboardNote = nil
        // conv copies one run's output itself. The app runs one conv a file, so with
        // several files it copies all the outputs at the end, as `--copy` would.
        let copyEach = copyResult && items.count == 1
        runTask = Task { @MainActor in
            for item in items {
                if Task.isCancelled { break }
                await run(item, conv: conv, extra: copyEach ? ["--copy"] : [])
            }
            if copyResult && items.count > 1 {
                let outputs = items.compactMap { item -> URL? in
                    if case .done(let out, _) = item.status { return out }
                    return nil
                }
                if !outputs.isEmpty {
                    Clipboard.copyFiles(outputs)
                    clipboardNote = "Copied \(outputs.count) files to the clipboard."
                }
            }
            isRunning = false
        }
    }

    /// Re-run one file after the user said yes to Replace or Convert anyway.
    @MainActor
    func retry(_ item: FileItem, extra: [String]) {
        guard let conv else { return }
        isRunning = true
        runTask = Task { @MainActor in
            await run(item, conv: conv, extra: extra + (copyResult ? ["--copy"] : []))
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
            if let c = resp.clipboard { clipboardNote = c.message }
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
                item.status = .failed(AppModel.friendly(err))
            }
        } catch {
            item.status = .failed(error.localizedDescription)
        }
    }
}
