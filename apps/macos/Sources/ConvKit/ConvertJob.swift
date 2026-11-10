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
    var ext: String { fileExt(url) }
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
    /// The flags this conv's --help lists, so a knob for one it doesn't have yet
    /// (--mute before it ships) can say so.
    var convFlags: Set<String> = []
    /// Every flag some pair lists as tuning.
    private(set) var listedFlags: Set<String> = []

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
        listedFlags = Set(found.values.flatMap { ($0.targets ?? []).flatMap(\.tuning) })
    }

    /// The long flags in a --help text.
    static func parseFlags(_ help: String) -> Set<String> {
        var out: Set<String> = []
        for line in help.split(separator: "\n") {
            let t = line.trimmingCharacters(in: .whitespaces)
            guard t.hasPrefix("-") else { continue }
            for word in t.split(separator: " ") where word.hasPrefix("--") {
                let name = word.split(separator: "[").first.map { String($0) } ?? String(word)
                out.insert(name.trimmingCharacters(in: CharacterSet(charactersIn: ",=<")))
            }
        }
        return out
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
    func targets(_ ext: String) -> [CapabilitiesResponse.Target] { byExt[ext]?.allTargets ?? [] }
    func reaches(_ ext: String, _ target: String) -> Bool { targets(ext).contains { $0.to == target } }

    /// Flags that apply when conv rewrites a file in its own format: what its sources of
    /// the same kind take on the way to it (any source when none is the same kind), and
    /// that the format also takes as a source. A PNG gets resize, colors and rotate but
    /// not quality; a GIF gets the time flags, from video; a JPG doesn't get GIF's stills.
    func writeTuning(_ ext: String) -> Set<String> {
        guard let me = byExt[ext], let name = me.format else { return [] }
        let sources = me.sources ?? []
        let sameKind = sources.filter { byExt[$0]?.kind == me.kind }
        var flags: Set<String> = []
        for s in sameKind.isEmpty ? sources : sameKind {
            if let t = byExt[s]?.targets?.first(where: { $0.to == name }) { flags.formUnion(t.tuning) }
        }
        let outgoing = Set((me.targets ?? []).flatMap(\.tuning))
        if !outgoing.isEmpty { flags.formIntersection(outgoing) }
        if name == "pdf" { flags.formUnion(Self.pdfOwn) }
        return flags
    }

    /// What conv does to a PDF kept as a PDF. `capabilities` lists pairs between
    /// formats, so these, which rewrite a PDF in place, aren't among its tuning flags.
    static let pdfOwn: Set<String> = ["--compress", "--max-size", "--password", "--restrict", "--unlock"]

    /// A newer flag `capabilities` may not list, and the one whose files it fits:
    /// muting goes wherever a new sound could.
    static let standIn: [String: String] = ["--mute": "--audio"]
}

/// Flags that make conv write a lone file in its own format (photo-50pct.jpg,
/// clip-rotated.mp4). Quality, colors, frame rate and CRF only refine one of these.
let keepFormatTriggers: Set<String> = [
    "--max-size", "--strip-metadata", "--start", "--end", "--duration", "--speed",
    "--resize", "--rotate", "--flip", "--crop", "--compress", "--denoise", "--audio",
    "--subtitles", "--password", "--unlock", "--mute",
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
    var flip = ""
    var crop = ""
    var mute = false
    var compress = false
    var every = ""
    /// "", "normal" or "strong".
    var denoise = ""
    var audioFile: URL?
    var mix = false
    /// How loud the new sound plays under the old one: "" is conv's 25%.
    var mixLevel = ""
    var subtitlesFile: URL?
    var pdfPages = ""
    var dpi = ""
    var lock = false
    var lockPassword = ""
    var lockAgain = ""
    var allowPrint = true
    var allowCopy = true
    var allowEdit = true
    var unlock = false
    /// The password that opens a locked PDF, for Unlock.
    var openPassword = ""
    /// Put the result on the clipboard (`--copy`).
    var copyResult = false
    var outputFolder: URL?

    private(set) var targets: [String] = []
    /// Flags conv takes for this set of files and format; nil before there are files.
    private(set) var tuning: Set<String>?
    private(set) var targetNotes: [String] = []

    private(set) var isRunning = false
    /// What the last Save wrote, for an editor's "Saved" line.
    private(set) var lastOutputs: [URL] = []
    /// The files came from a chain: they sit in its scratch folder, and the result saves
    /// in `destination` (the chain's starting folder) under the name conv would give it
    /// beside its input, never as `protected` (the file the chain started from).
    var fromChain = false
    var destination: URL?
    var protected: URL?
    var isEditor: Bool { preset.editor != nil }
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
        unlock = preset.alwaysUnlock
        if preset.startsOn.contains(.compress) { compress = true }
        if preset.startsOn.contains(.denoise) { denoise = "normal" }
        if preset.startsOn.contains(.lock) { lock = true }
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
        lastOutputs = []
        Task { await refreshTargets() }
    }

    /// An editor holds one file: a new one takes its place.
    @MainActor
    func replace(with url: URL) { replace(with: [url]) }

    @MainActor
    func replace(with urls: [URL]) {
        files = []
        lastOutputs = []
        add(urls)
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
        case .keep:
            format = "same"
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
        if preset.skipTargets.contains(to) { return false }
        if preset.extraTargets.contains(to) { return true }
        guard case .pick(let kinds, _, _) = preset.target, !kinds.isEmpty else { return true }
        return caps.kind(to).map(kinds.contains) ?? false
    }

    var allowsSame: Bool {
        switch preset.target {
        case .pick(_, let allowSame, _): allowSame
        case .keep: true
        case .fixed: false
        }
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
        if !caps.convFlags.isEmpty && !caps.convFlags.contains(flag) { return false }
        guard let tuning else { return !isGeneral }
        // A flag conv has but no pair lists yet goes where its neighbour does.
        if !caps.listedFlags.contains(flag), let like = CapsStore.standIn[flag] { return tuning.contains(like) }
        return tuning.contains(flag)
    }

    /// Why a knob is greyed out, or nil when it isn't.
    func reason(_ flag: String) -> String? {
        if !caps.convFlags.isEmpty && !caps.convFlags.contains(flag) {
            return "This conv can't do that yet: it has no \(flag)."
        }
        if applies(flag) {
            if flag == "--mute" && audioFile != nil { return "Mute or a new sound, not both." }
            if flag == "--audio" && mute { return "Mute or a new sound, not both." }
            if flag == "--denoise" && mute { return "There's no sound left to clean." }
            if flag == "--max-size" && audioFile != nil {
                return "conv adds new sound first and sizes after: use Then › Compress Video."
            }
            if flag == "--duration" && !end.isEmpty { return "Set an end or a length, not both." }
            if flag == "--end" && !duration.isEmpty { return "Set an end or a length, not both." }
            if flag == "--password" && unlock && !preset.alwaysUnlock && !lock { return "Lock or unlock, not both." }
            if flag == "--unlock" && lock && !preset.alwaysUnlock { return "Lock or unlock, not both." }
            if flag == "--dpi" && !resize.isEmpty { return "Resize sets the size instead." }
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
        check(strip && !preset.alwaysStrip, "--strip-metadata", "Remove metadata")
        check(!rotate.isEmpty, "--rotate", "Turn")
        check(!flip.isEmpty, "--flip", "Flip")
        check(!crop.isEmpty, "--crop", "Crop")
        check(mute, "--mute", "Mute")
        check(compress && !preset.startsOn.contains(.compress), "--compress", "Compress")
        check(!every.isEmpty, "--every", "Every")
        check(!denoise.isEmpty && !preset.startsOn.contains(.denoise), "--denoise", "Remove noise")
        check(audioFile != nil, "--audio", "Audio")
        check(subtitlesFile != nil, "--subtitles", "Subtitles")
        check(!pdfPages.isEmpty, "--pages", "Pages")
        check(!dpi.isEmpty, "--dpi", "Resolution")
        check(lock && !lockPassword.isEmpty && !preset.startsOn.contains(.lock), "--password", "Lock")
        check(unlock && !preset.alwaysUnlock, "--unlock", "Unlock")
        return out
    }

    // MARK: Arguments

    /// conv's argv for one file, without --json/--dry-run/-y. `out` names the file to
    /// write and `folder` the folder, for chains.
    func args(for item: FileItem, writing out: URL? = nil, into folder: URL? = nil) -> [String] {
        var a = [item.url.path]
        if let out {
            a.append(out.path)
        } else if format != "same" && !format.isEmpty {
            a.append("." + format)
        }
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
        add("--flip", flip)
        add("--crop", crop.replacingOccurrences(of: " ", with: ""))
        if compress && applies("--compress") { a.append("--compress") }
        add("--every", every)
        if mute && applies("--mute") && audioFile == nil { a.append("--mute") }
        if !denoise.isEmpty && applies("--denoise") && !mute {
            a.append(denoise == "strong" ? "--denoise=strong" : "--denoise")
        }
        if let f = audioFile, applies("--audio"), !mute {
            a += ["--audio", f.path]
            if mix && applies("--mix") { a.append(mixLevel.isEmpty ? "--mix" : "--mix=\(mixLevel)") }
        }
        if let f = subtitlesFile, applies("--subtitles") { a += ["--subtitles", f.path] }
        add("--pages", pdfPages.replacingOccurrences(of: " ", with: ""))
        if resize.isEmpty { add("--dpi", dpi) }
        if locking {
            a.append("--password")
            let off = [("print", allowPrint), ("copy", allowCopy), ("edit", allowEdit)].filter { !$0.1 }.map(\.0)
            if !off.isEmpty && applies("--restrict") { a += ["--restrict", off.joined(separator: ",")] }
        }
        if unlock && applies("--unlock") && !locking { a.append("--unlock") }
        if out != nil {
            // The path says where.
        } else if let folder {
            a += ["-o", folder.path]
        } else if let dir = outputFolder {
            a += ["-o", dir.path]
        } else if item.pasted {
            a += ["-o", Clipboard.pastedOutputFolder.path]
        }
        return a
    }

    private var locking: Bool { lock && !lockPassword.isEmpty && applies("--password") }

    /// conv reads a password from CONVKIT_PASSWORD, never from its arguments: the new
    /// one when locking, the one that opens the file when unlocking.
    var passwordEnv: [String: String] {
        if locking { return ["CONVKIT_PASSWORD": lockPassword] }
        if unlock && !openPassword.isEmpty && applies("--unlock") { return ["CONVKIT_PASSWORD": openPassword] }
        return [:]
    }

    /// Where a run writes in a chain. A file is named as conv would name it beside its
    /// input (`-o` elsewhere keeps the bare name, which could be the original's); a
    /// folder of stills goes in with `-o`.
    private func target(for item: FileItem, in folder: URL?) -> (writing: URL?, into: URL?) {
        guard let folder else { return (nil, nil) }
        if let planned = item.plannedOutput, !planned.pathExtension.isEmpty {
            return (folder.appendingPathComponent(planned.lastPathComponent), nil)
        }
        return (nil, folder)
    }

    /// The chain's folder, unless the user picked one.
    private var chainFolder: URL? { outputFolder == nil ? destination : nil }

    /// The command for the first file, for the "Command" disclosure.
    var commandPreview: String {
        guard let first = fitting.first else { return "" }
        let t = target(for: first, in: chainFolder)
        let parts = ["conv"] + args(for: first, writing: t.writing, into: t.into).map { arg in
            arg.contains(where: { " '\"$`\\()[]&;|<>*?!#~".contains($0) })
                ? "'" + arg.replacingOccurrences(of: "'", with: "'\\''") + "'"
                : arg
        }
        let env = passwordEnv.isEmpty ? "" : "CONVKIT_PASSWORD=… "
        return env + parts.joined(separator: " ")
    }

    /// Changes whenever any setting does, so the form can re-run the preview.
    var settingsKey: String {
        [format, maxSize, resize, "\(upscale)", colors, fps, crf, duration, "\(useQuality)\(Int(quality))",
         speed, "\(shiftPitch)", start, end, "\(strip)", rotate, flip, crop, "\(mute)", "\(compress)", every, denoise,
         audioFile?.path ?? "", "\(mix)", mixLevel, subtitlesFile?.path ?? "", pdfPages, dpi,
         "\(lock)", lockPassword, lockAgain, "\(allowPrint)\(allowCopy)\(allowEdit)", "\(unlock)", openPassword,
         outputFolder?.path ?? ""].joined(separator: "|")
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
        other.flip = flip
        other.crop = crop
        other.mute = mute
        other.compress = compress
        other.every = every
        other.denoise = denoise
        other.audioFile = audioFile
        other.mix = mix
        other.mixLevel = mixLevel
        other.subtitlesFile = subtitlesFile
        other.pdfPages = pdfPages
        other.dpi = dpi
        other.lock = lock
        other.lockPassword = lockPassword
        other.lockAgain = lockAgain
        other.allowPrint = allowPrint
        other.allowCopy = allowCopy
        other.allowEdit = allowEdit
        other.unlock = unlock
        other.openPassword = openPassword
        other.copyResult = copyResult
        other.outputFolder = outputFolder
        if format != "same" && !format.isEmpty { other.choose(format: format) }
        other.add(fitting.map(\.url))
    }

    // MARK: Preview

    /// Called on every option change: asks conv (dry run) what each file would become.
    func optionsChanged() {
        updateTuning()
        // An editor saves again after a change, so what it saved last isn't "done".
        if isEditor { for item in files where item.isDone { item.status = .idle } }
        previewTask?.cancel()
        let items = fitting
        guard let conv, !items.isEmpty else { return }
        let argv = items.map { args(for: $0) }
        let env = passwordEnv
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
                guard let out = try? await Conv.run(conv, args + ["--dry-run", "--json", "--no-install"], env: env)
                else { continue }
                guard !Task.isCancelled else { return }
                apply(dryRun: out, to: item)
            }
        }
    }

    /// conv would refuse these, so say what to do instead.
    private func nothingToDo(_ args: [String]) -> String? {
        if format.isEmpty { return "conv can't make anything this tool offers from this file." }
        if lock && !lockPassword.isEmpty && lockPassword != lockAgain && applies("--password") {
            return lockAgain.isEmpty ? "Type the password again." : "The two passwords don't match."
        }
        // --denoise=strong is still --denoise.
        let flags = args.map { String($0.split(separator: "=", maxSplits: 1).first ?? "") }
        if format == "same" && !flags.contains(where: keepFormatTriggers.contains) { return preset.needs }
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
        item.plannedOutput = (entry?.plan?.output ?? entry?.output).map {
            URL(fileURLWithPath: $0, relativeTo: item.url.deletingLastPathComponent()).standardizedFileURL
        }
        if let folder = chainFolder, let planned = item.plannedOutput, let protected,
           folder.appendingPathComponent(planned.lastPathComponent).standardizedFileURL == protected.standardizedFileURL {
            item.problem = "That would replace \(protected.lastPathComponent), the file this chain started from. Pick another format, or Save In another folder."
            item.plannedOutput = nil
            return
        }
        var warnings = entry?.plan?.warnings ?? entry?.warnings ?? []
        if let w = entry?.plan?.sizing?.warning {
            warnings.append(w + (entry?.plan?.sizing?.suggested.map { " Try \($0)." } ?? ""))
        }
        item.warnings = warnings
    }

    // MARK: Run

    var readyCount: Int {
        fitting.filter { $0.problem == nil && $0.plannedOutput != nil && !$0.isDone }.count
    }

    /// Runs every ready file into a scratch folder and gives back what it wrote, for
    /// the next tool in a chain. The rows go back to planned, since nothing was saved.
    @MainActor
    func runForChain(into folder: URL) async -> [URL] {
        guard let conv else { return [] }
        let items = fitting.filter { $0.problem == nil && $0.plannedOutput != nil }
        isRunning = true
        defer { isRunning = false }
        var outputs: [URL] = []
        for item in items {
            await run(item, conv: conv, extra: [], into: folder, chainStep: true)
            if case .done(let out, _) = item.status {
                outputs.append(out)
                item.status = .idle
            } else {
                return []
            }
        }
        return outputs
    }

    @MainActor
    func runAll() {
        guard let conv else { return }
        let items = fitting.filter { $0.problem == nil && $0.plannedOutput != nil && !$0.isDone }
        isRunning = true
        clipboardNote = nil
        lastOutputs = []
        // conv copies one run's output itself. The app runs one conv a file, so with
        // several files it copies all the outputs at the end, as `--copy` would.
        let copyEach = copyResult && items.count == 1
        runTask = Task { @MainActor in
            for item in items {
                if Task.isCancelled { break }
                await run(item, conv: conv, extra: copyEach ? ["--copy"] : [])
                if case .done(let out, _) = item.status { lastOutputs.append(out) }
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
            if case .done(let out, _) = item.status { lastOutputs = [out] }
            isRunning = false
        }
    }

    @MainActor
    func cancel() {
        runTask?.cancel()
    }

    @MainActor
    private func run(_ item: FileItem, conv: URL, extra: [String], into folder: URL? = nil, chainStep: Bool = false) async {
        let t = target(for: item, in: chainStep ? folder : chainFolder)
        item.status = .running
        item.notes = []
        // --yes answers conv's prompts, so it only goes in when the user chose Convert anyway.
        let guardFlags = extra.contains("--yes") ? [] : ["--no-install"]
        do {
            let out = try await Conv.run(conv, args(for: item, writing: t.writing, into: t.into) + ["--json"] + guardFlags + extra,
                                         env: passwordEnv)
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
                item.status = .done(output: URL(fileURLWithPath: output, relativeTo: item.url.deletingLastPathComponent()).standardizedFileURL,
                                    bytes: r.bytes)
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
