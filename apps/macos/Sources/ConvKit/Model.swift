import AppKit
import Foundation
import Observation
import PDFKit

/// Where a save panel opens: the chain's starting folder while a chain is on.
enum SaveDefaults {
    @MainActor static var folder: URL?
}

/// Everything the app writes to temp goes in here, and launch empties it. conv's own
/// temp files (convkit-lo-…) sit beside it, so a conv in a terminal keeps them.
let appTemp = FileManager.default.temporaryDirectory.appendingPathComponent("ConvKit", isDirectory: true)

/// What the detail pane shows.
enum Page: Hashable {
    case home
    case tool(ToolID)
}

/// The app's state: the page showing, the conv in use, and every tool's own model.
@Observable
final class AppModel {
    var page: Page = .home {
        didSet { if case .tool(let id) = page { noteRecent(id) } }
    }
    /// The search in the toolbar. Its matches drop down under the top bar.
    var search = ""
    /// The section whose dropdown is open in the top bar.
    var menuOpen: Category?
    /// Where an editor opens when a search shortcut ("Crop Video") led to it.
    var editorFocus: EditorSection?
    /// Steps run one into the next; nil when the tool showing isn't part of one.
    private(set) var chain: Chain?

    let caps = CapsStore()
    /// Every convert-style tool's files and settings, made up front so views never
    /// create state while drawing.
    let jobs: [ToolID: ConvertJob]
    let qr = QRModel()
    let ascii = AsciiModel()
    let merge = MergeModel(mode: .pdfs)
    let photos = MergeModel(mode: .photos)
    let joinVideos = MergeModel(mode: .videos)
    let joinAudio = MergeModel(mode: .audio)
    let split = SplitModel()
    let pages = PagesModel()
    let makeArchive = MakeArchiveModel()
    let openArchive = OpenArchiveModel()
    let speak = SpeakModel()
    let beautify = BeautifyModel()
    let colors = ColorsModel()
    let wifi = WifiModel()
    /// The small text and developer tools, one model each.
    let quick: [ToolID: QuickModel]
    let scan = ScanModel()
    let data = DataModel()
    let backends = BackendsModel()
    let formats = FormatsModel()
    let changelog = ChangelogModel()

    /// Subcommands the conv in use offers, from its --help.
    private(set) var subcommands: Set<String> = []
    private(set) var recent: [ToolID] = (UserDefaults.standard.stringArray(forKey: "recentTools") ?? [])
        .compactMap(ToolID.init(rawValue:))

    /// Files dropped on Home, waiting for the user to pick a tool for them.
    private(set) var dropped: [URL] = []
    private(set) var suggestions: [ToolID] = []

    private(set) var convURL: URL?
    private(set) var convVersion = ""
    private(set) var setupProblem: String?
    /// Why the last Paste in a non-convert tool brought nothing in.
    var pasteProblem: String?
    var convOverride: String = UserDefaults.standard.string(forKey: "convPath") ?? "" {
        didSet {
            UserDefaults.standard.set(convOverride, forKey: "convPath")
            Task { await connect() }
        }
    }

    init() {
        var jobs: [ToolID: ConvertJob] = [:]
        for id in ToolID.allCases where id.preset != nil && id.shortcut == nil {
            jobs[id] = ConvertJob(tool: id, caps: caps)
        }
        self.jobs = jobs
        var quick: [ToolID: QuickModel] = [:]
        for id in ToolID.allCases {
            if let spec = id.quickSpec { quick[id] = QuickModel(tool: id, spec: spec) }
        }
        self.quick = quick
    }

    func mergeModel(_ id: ToolID) -> MergeModel {
        switch id {
        case .photosToPDF: photos
        case .joinVideos: joinVideos
        case .joinAudio: joinAudio
        default: merge
        }
    }

    // MARK: Setup

    /// Each keystroke in Settings connects again; only the newest one sets things up.
    private var connects = 0

    @MainActor
    func connect() async {
        connects += 1
        let mine = connects
        guard let url = Conv.locate(override: convOverride) else {
            convURL = nil
            setupProblem = "Can't find conv. Install it, or set its path in Settings."
            return
        }
        convURL = url
        caps.conv = url
        for m in [merge, photos, joinVideos, joinAudio] { m.conv = url }
        for m in quick.values { m.conv = url }
        qr.conv = url
        ascii.conv = url
        split.conv = url
        pages.conv = url
        makeArchive.conv = url
        openArchive.conv = url
        speak.conv = url
        beautify.conv = url
        colors.conv = url
        wifi.conv = url
        scan.conv = url
        data.conv = url
        backends.conv = url
        formats.conv = url
        changelog.conv = url
        setupProblem = nil
        let version = try? await Conv.run(url, ["--version"])
        let helpOut = try? await Conv.run(url, ["--help"])
        guard mine == connects else { return }
        if let out = version {
            convVersion = String(decoding: out.stdout, as: UTF8.self)
                .trimmingCharacters(in: .whitespacesAndNewlines)
        }
        if let out = helpOut {
            let help = String(decoding: out.stdout, as: UTF8.self)
            subcommands = Self.parseSubcommands(help)
            caps.convFlags = CapsStore.parseFlags(help)
        }
        await caps.load()
        guard mine == connects else { return }
        // Pictures that go into a PDF one a page, alone or among PDFs.
        photos.accepts = Set((caps.byExt["pdf"]?.sources ?? []).filter { caps.kind($0) == "image" })
        merge.accepts = photos.accepts.union(["pdf"])
        let ofKind = { (k: String) in Set(self.caps.byExt.filter { $0.value.kind == k }.map(\.key)) }
        joinVideos.accepts = ofKind("video")
        joinAudio.accepts = ofKind("audio").union(ofKind("video"))
        if case .tool(let id) = page, !available(id) { page = .home }
        for job in jobs.values { await job.refreshTargets() }
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

    // MARK: Finding tools

    /// A tool shows when the conv in use can do it: its subcommand is there, or some
    /// format reaches its fixed target.
    func available(_ id: ToolID) -> Bool {
        if let s = id.shortcut {
            if id == .muteVideo && !caps.convFlags.contains("--mute") { return false }
            return available(s.editor)
        }
        if let sub = id.subcommand, !subcommands.contains(sub) { return false }
        if caps.byExt.isEmpty { return id.preset == nil || id == .convert }
        if let t = id.preset?.fixedTarget { return caps.byExt.values.contains { $0.targets?.contains { $0.to == t } ?? false } }
        if id == .convertData { return caps.byExt["json"] != nil }
        if id == .photosToPDF { return !photos.accepts.isEmpty }
        if id == .makeArchive || id == .openArchive { return caps.byExt["zip"] != nil }
        if id.preset?.exts == ["pdf"] { return caps.byExt["pdf"] != nil }
        return true
    }

    /// The tools in a section that match the search. Shortcuts into the editors only
    /// show in a search, so "crop" finds Crop Video and Crop Image.
    func tools(in category: Category) -> [ToolID] {
        ToolID.allCases.filter {
            $0.category == category && ($0.shortcut == nil || isSearching) && available($0) && matches($0)
        }
    }

    /// A section's tools as its dropdown in the top bar lists them: all of them, with
    /// the shortcuts left to search.
    func menuTools(in category: Category) -> [ToolID] {
        ToolID.allCases.filter { $0.category == category && $0.shortcut == nil && available($0) }
    }

    /// Shows a tool, or the editor a shortcut stands for, at its part. A search is
    /// done once something is picked, so it's cleared.
    @MainActor
    func show(_ id: ToolID) {
        search = ""
        if let s = id.shortcut {
            editorFocus = s.section
            page = .tool(s.editor)
        } else {
            editorFocus = nil
            page = .tool(id)
        }
    }

    var popular: [ToolID] { ToolID.popular.filter(available) }
    var recentTools: [ToolID] { recent.filter(available) }
    var isSearching: Bool { !search.trimmingCharacters(in: .whitespaces).isEmpty }

    /// Every word of the search is in the tool's name, description or keywords, or is a
    /// format the tool takes or makes ("heic" finds the picture tools).
    func matches(_ id: ToolID) -> Bool {
        let words = search.lowercased().split(whereSeparator: { $0 == " " || $0 == "," }).map(String.init)
        guard !words.isEmpty else { return true }
        let text = id.searchText
        return words.allSatisfy { word in
            if text.contains(word) { return true }
            let ext = word.hasPrefix(".") ? String(word.dropFirst()) : word
            guard caps.byExt[ext] != nil else { return false }
            // A join takes several, but one format is enough to say it takes it.
            if case .merge = id.kind { return available(id) && mergeModel(id).accepts.contains(ext) }
            return fits(id, exts: [ext])
        }
    }

    /// Search matches in top bar order, for Return in the search field.
    var searchResults: [ToolID] { Category.allCases.flatMap(tools(in:)) }

    private func noteRecent(_ id: ToolID) {
        recent = Array(([id] + recent.filter { $0 != id }).prefix(6))
        UserDefaults.standard.set(recent.map(\.rawValue), forKey: "recentTools")
    }

    // MARK: What fits a file

    /// Whether a tool can do something with all of these files.
    func fits(_ id: ToolID, exts: [String], folder: Bool = false) -> Bool {
        guard available(id), id.shortcut == nil, !exts.isEmpty || folder else { return false }
        if folder { return [.scan, .convert, .mergePDF, .makeArchive, .info].contains(id) }
        let kinds = exts.map { caps.kind($0) }
        let one = exts.count == 1
        switch id.kind {
        case .convert(let p):
            // An editor works on one file at a time.
            if p.editor != nil && !one { return false }
            for (ext, kind) in zip(exts, kinds) {
                guard p.takes(ext, kind: kind) else { return false }
                if let t = p.fixedTarget {
                    guard caps.reaches(ext, t) else { return false }
                } else if case .pick(let tk, let allowSame, _) = p.target {
                    let offered = caps.targets(ext).contains { tk.isEmpty || caps.kind($0.to).map(tk.contains) == true }
                    let keeps = allowSame && !caps.writeTuning(ext).isDisjoint(with: p.ownFlags)
                    guard offered || keeps else { return false }
                } else if case .keep = p.target {
                    guard !caps.writeTuning(ext).isDisjoint(with: p.ownFlags) else { return false }
                }
            }
            return true
        case .merge:
            let m = mergeModel(id)
            guard exts.allSatisfy(m.accepts.contains) else { return false }
            switch id {
            case .mergePDF: return exts.count >= 2 && exts.contains("pdf")
            case .photosToPDF: return true
            case .joinAudio: return exts.count >= 2 && kinds.contains("audio")
            default: return exts.count >= 2
            }
        case .split, .pages:
            return exts == ["pdf"]
        case .quick:
            guard one else { return false }
            switch id {
            case .info, .hash: return true
            case .base64: return kinds[0] == "image"
            case .readQR: return kinds[0] == "image"
            default: return false
            }
        case .speak:
            return one && ToolID.speakExts.contains(exts[0])
        case .beautify, .colors:
            return one && kinds[0] == "image"
        case .makeArchive:
            return true
        case .openArchive:
            return one && OpenArchiveModel.exts.contains(exts[0])
        case .ascii:
            return exts.count == 1 && (kinds[0] == "image" || kinds[0] == "video")
        case .data:
            return exts.count == 1 && ToolID.dataExts.contains(exts[0])
        case .scan, .qr, .wifi, .backends, .formats, .changelog:
            return false
        }
    }

    /// Files dropped or opened on Home: list the tools that fit them, the files' own
    /// section first and Convert Anything last.
    @MainActor
    func suggest(for urls: [URL]) async {
        guard !urls.isEmpty else { return }
        let fm = FileManager.default
        let folder = urls.contains { var d: ObjCBool = false; return fm.fileExists(atPath: $0.path, isDirectory: &d) && d.boolValue }
        let exts = urls.map(fileExt)
        await caps.ensure(Set(exts))
        let kinds = Set(exts.compactMap(caps.kind))
        dropped = urls
        defer { page = .home }
        suggestions = toolsFitting(urls, exts: exts, kinds: kinds, folder: folder)
    }

    /// The tools that fit these files: their own section first, then the rest, then
    /// the ones that take any file.
    func toolsFitting(_ urls: [URL], exts: [String], kinds: Set<String>, folder: Bool) -> [ToolID] {
        let fm = FileManager.default
        if folder {
            // A folder: see what's in it, convert all of it, or join its PDFs.
            let hasPDFs = urls.contains { url in
                ((try? fm.contentsOfDirectory(atPath: url.path)) ?? []).contains { $0.lowercased().hasSuffix(".pdf") }
            }
            let hasPictures = urls.contains { url in
                ((try? fm.contentsOfDirectory(atPath: url.path)) ?? []).contains { photos.accepts.contains(($0 as NSString).pathExtension.lowercased()) }
            }
            return [.scan, .makeArchive, .convert].filter(available)
                + (hasPDFs && available(.mergePDF) ? [.mergePDF] : [])
                + (hasPictures && available(.photosToPDF) ? [.photosToPDF] : [])
                + (urls.count == 1 && available(.info) ? [.info] : [])
        }
        // Tools that take any file go last, after the ones made for this kind.
        let anyFile: [ToolID] = [.convert, .info, .hash, .base64, .makeArchive]
        // Unlock only for a PDF that has a password.
        let locked = urls.contains { $0.pathExtension.lowercased() == "pdf" && PDFDocument(url: $0)?.isEncrypted == true }
        let fitting = ToolID.allCases.filter { !anyFile.contains($0) && fits($0, exts: exts) && ($0 != .unlockPDF || locked) }
        let own = fitting.filter { $0.category.kind.map(kinds.contains) ?? false }
        return own + fitting.filter { !own.contains($0) }
            + anyFile.filter { $0 == .convert ? available($0) : fits($0, exts: exts) }
    }

    // MARK: Chains

    /// Steps run one into the next. Each step's result waits in a scratch folder, and
    /// the last step saves next to the file the chain started from.
    struct Chain {
        var origin: URL
        var steps: [ToolID]
        let scratch: URL
        var destination: URL { origin.deletingLastPathComponent() }
    }

    /// What can come next after a step that will make these files (planned names, so
    /// a folder of stills is a folder).
    func nextTools(after tool: ToolID, making outputs: [URL]) -> [ToolID] {
        guard !outputs.isEmpty else { return [] }
        let folder = outputs.contains { $0.pathExtension.isEmpty }
        let exts = outputs.map(fileExt)
        let kinds = Set(exts.compactMap(caps.kind))
        return toolsFitting(outputs, exts: exts, kinds: kinds, folder: folder).filter { $0 != tool }
    }

    /// Runs a convert-style step into the chain's scratch folder and opens the next tool
    /// with what it made.
    @MainActor
    func chain(_ job: ConvertJob, to next: ToolID) async {
        let origin = (job.fromChain ? chain?.origin : nil) ?? job.fitting.first?.url
        guard let origin else { return }
        let place = stepFolder(origin: origin, from: job.tool)
        let outputs = await job.runForChain(into: place.step)
        continueChain(from: job.tool, origin: origin, place: place, outputs: outputs, to: next)
    }

    /// The same for a step that makes one file itself (a join, Arrange Pages).
    @MainActor
    func chain(from tool: ToolID, origin: URL, fromChain: Bool, make: (URL) async -> URL?, to next: ToolID) async {
        let origin = fromChain ? (chain?.origin ?? origin) : origin
        let place = stepFolder(origin: origin, from: tool)
        let out = await make(place.step)
        continueChain(from: tool, origin: origin, place: place, outputs: out.map { [$0] } ?? [], to: next)
    }

    /// Where a step's result goes. A step already in the chain (the user went back to
    /// it) keeps the chain's folder, so the steps before it still have their files.
    private struct StepPlace { let scratch: URL; let step: URL; let keep: Int? }

    @MainActor
    private func stepFolder(origin: URL, from tool: ToolID) -> StepPlace {
        let tag = String(UUID().uuidString.prefix(8))
        let place: StepPlace
        if let chain, chain.origin == origin, let i = chain.steps.lastIndex(of: tool) {
            place = StepPlace(scratch: chain.scratch, step: chain.scratch.appendingPathComponent("step-\(tag)"), keep: i)
        } else {
            let scratch = appTemp.appendingPathComponent("convkit-chain-\(tag)", isDirectory: true)
            place = StepPlace(scratch: scratch, step: scratch.appendingPathComponent("step-1"), keep: nil)
        }
        try? FileManager.default.createDirectory(at: place.step, withIntermediateDirectories: true)
        return place
    }

    @MainActor
    private func continueChain(from tool: ToolID, origin: URL, place: StepPlace, outputs: [URL], to next: ToolID) {
        guard !outputs.isEmpty else {
            // Nothing made: the rows say why, and there's no chain to keep.
            if place.keep == nil { try? FileManager.default.removeItem(at: place.scratch) }
            return
        }
        if let keep = place.keep, var c = chain {
            c.steps = Array(c.steps.prefix(keep + 1))
            chain = c
        } else {
            endChain()
            chain = Chain(origin: origin, steps: [tool], scratch: place.scratch)
        }
        chain?.steps.append(next)
        SaveDefaults.folder = chain?.destination
        open(next, with: outputs, chained: true)
    }

    /// Back to working on files one tool at a time. Scratch results are thrown away.
    @MainActor
    func endChain() {
        guard let old = chain else { return }
        chain = nil
        SaveDefaults.folder = nil
        for job in jobs.values where job.fromChain {
            job.fromChain = false
            job.destination = nil
            job.protected = nil
            job.optionsChanged()
        }
        if split.outputFolder == old.destination { split.outputFolder = nil }
        if pages.outputFolder == old.destination { pages.outputFolder = nil }
        if makeArchive.outputFolder == old.destination { makeArchive.outputFolder = nil }
        try? FileManager.default.removeItem(at: old.scratch)
    }

    @MainActor
    func clearDropped() {
        dropped = []
        suggestions = []
    }

    /// Finder's Open With, the Dock or the command line: into the tool showing when it
    /// fits, otherwise Home lists the tools that do.
    @MainActor
    func open(_ urls: [URL]) {
        guard !urls.isEmpty else { return }
        endChain()
        if case .tool(let id) = page {
            let exts = urls.map(fileExt)
            if id == .convert || fits(id, exts: exts) {
                open(id, with: urls)
                return
            }
        }
        Task { await suggest(for: urls) }
    }

    /// Shows a tool with these files in it. In a chain, the files are the last step's
    /// results, and the tool saves where the chain started.
    @MainActor
    func open(_ id: ToolID, with urls: [URL], chained: Bool = false) {
        var id = id
        var focus: EditorSection?
        if let s = id.shortcut {
            id = s.editor
            focus = s.section
        } else if id.preset?.editor != nil, page == .tool(id) {
            focus = editorFocus
        }
        // A file goes to the editor for its kind: Remove Noise or Edit Audio with a
        // video opens Edit Video.
        if id.preset?.editor != nil, let first = urls.first,
           let editor = ToolID.editor(for: caps.kind(fileExt(first))), available(editor) {
            id = editor
        }
        editorFocus = focus
        page = .tool(id)
        guard let first = urls.first else { return }
        let destination = chained ? chain?.destination : nil
        switch id.kind {
        case .convert:
            // A run reads its files, and where they go, as it gets to each.
            guard let job = jobs[id], !job.isRunning else { break }
            // An editor holds one file; a chained step holds just what the step before made.
            if job.isEditor { job.replace(with: first) } else if chained { job.replace(with: urls) } else { job.add(urls) }
            job.fromChain = chained
            job.destination = chained ? destination : nil
            job.protected = chained ? chain?.origin : nil
        case .merge: mergeModel(id).add(urls)
        case .split:
            split.load(first)
            if chained { split.outputFolder = destination }
        case .pages:
            pages.load(first)
            if chained { pages.outputFolder = destination }
        case .ascii: ascii.input = first; ascii.changed()
        case .data: data.load(first)
        case .scan: Task { await scan.scan(first) }
        case .quick: quick[id]?.load(first)
        case .speak: speak.load(first)
        case .beautify: beautify.load(first)
        case .colors: colors.load(first)
        case .makeArchive:
            makeArchive.add(urls)
            if chained { makeArchive.outputFolder = destination }
        case .openArchive:
            openArchive.load(first)
            if chained { openArchive.outputFolder = destination }
        case .qr, .wifi, .backends, .formats, .changelog: break
        }
        if urls == dropped { clearDropped() }
    }

    /// Carries a focused tool's files and settings into Convert Anything.
    @MainActor
    func moreOptions(from job: ConvertJob) {
        guard let all = jobs[.convert], job !== all else { return }
        job.copySettings(to: all)
        all.fromChain = job.fromChain
        show(.convert)
    }

    /// Paste for the tools that aren't Convert: the clipboard's files or picture, or none and a problem.
    @MainActor
    func pasteURLs() async -> [URL] {
        guard let conv = convURL else { return [] }
        switch await Clipboard.take(conv: conv) {
        case .success(let urls):
            pasteProblem = nil
            return urls
        case .failure(let problem):
            pasteProblem = problem.message
            return []
        }
    }

    /// conv's messages are written for a terminal; soften the CLI bits.
    static func friendly(_ err: ConvError) -> String {
        switch err.code {
        case "unsupported_pair":
            return "conv can't make that from this file."
        case "output_exists":
            // "/path/x.pdf exists; pass -y to overwrite" -> "x.pdf already exists."
            let path = err.message.components(separatedBy: " exists;").first ?? err.message
            return "\((path as NSString).lastPathComponent) already exists."
        case "backend_missing":
            return err.message + " Backends, under conv in the top bar, shows how to install it."
        case "password_protected":
            return err.message.hasPrefix("wrong password")
                ? "That password doesn't open it."
                : "It's locked with a password. Type the password to open it."
        default:
            return err.message
                .replacingOccurrences(of: "; run `conv capabilities` to list supported pairs", with: "")
        }
    }
}
