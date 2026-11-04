import AppKit
import Foundation
import Observation

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
    /// The sidebar's search, which also filters Home.
    var search = ""

    let caps = CapsStore()
    /// Every convert-style tool's files and settings, made up front so views never
    /// create state while drawing.
    let jobs: [ToolID: ConvertJob]
    let qr = QRModel()
    let ascii = AsciiModel()
    let trimVideo = TrimModel()
    let trimAudio = TrimModel()
    let merge = MergeModel(mode: .pdfs)
    let photos = MergeModel(mode: .photos)
    let split = SplitModel()
    let scan = ScanModel()
    let data = DataModel()
    let backends = BackendsModel()
    let formats = FormatsModel()
    let changelog = ChangelogModel()

    /// Subcommands the conv in use offers, from its --help.
    private(set) var subcommands: Set<String> = []
    private(set) var recent: [ToolID] = (UserDefaults.standard.stringArray(forKey: "recentTools") ?? [])
        .compactMap(ToolID.init(rawValue:))
    /// Sections the user folded away in the sidebar.
    var collapsed: Set<Category> = Set((UserDefaults.standard.stringArray(forKey: "collapsedSections") ?? [])
        .compactMap(Category.init(rawValue:))) {
        didSet { UserDefaults.standard.set(collapsed.map(\.rawValue), forKey: "collapsedSections") }
    }

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
        for id in ToolID.allCases where id.preset != nil {
            jobs[id] = ConvertJob(tool: id, caps: caps)
        }
        self.jobs = jobs
    }

    // MARK: Setup

    @MainActor
    func connect() async {
        guard let url = Conv.locate(override: convOverride) else {
            convURL = nil
            setupProblem = "Can't find conv. Install it, or set its path in Settings."
            return
        }
        convURL = url
        caps.conv = url
        for m in [trimVideo, trimAudio] { m.conv = url }
        for m in [merge, photos] { m.conv = url }
        qr.conv = url
        ascii.conv = url
        split.conv = url
        scan.conv = url
        data.conv = url
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
        await caps.load()
        // Pictures that go into a PDF one a page.
        photos.accepts = Set((caps.byExt["pdf"]?.sources ?? []).filter { caps.kind($0) == "image" })
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
        if let sub = id.subcommand, !subcommands.contains(sub) { return false }
        if caps.byExt.isEmpty { return id.preset == nil || id == .convert }
        if let t = id.preset?.fixedTarget { return caps.byExt.values.contains { $0.targets?.contains { $0.to == t } ?? false } }
        if id == .convertData { return caps.byExt["json"] != nil }
        if id == .photosToPDF { return !photos.accepts.isEmpty }
        return true
    }

    /// The tools in a section that match the search.
    func tools(in category: Category) -> [ToolID] {
        ToolID.allCases.filter { $0.category == category && available($0) && matches($0) }
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
            return fits(id, exts: [ext])
        }
    }

    /// Search matches in sidebar order, for Return in the search field.
    var searchResults: [ToolID] { Category.allCases.flatMap(tools(in:)) }

    private func noteRecent(_ id: ToolID) {
        recent = Array(([id] + recent.filter { $0 != id }).prefix(6))
        UserDefaults.standard.set(recent.map(\.rawValue), forKey: "recentTools")
    }

    // MARK: What fits a file

    /// Whether a tool can do something with all of these files.
    func fits(_ id: ToolID, exts: [String], folder: Bool = false) -> Bool {
        guard available(id), !exts.isEmpty || folder else { return false }
        if folder { return [.scan, .convert, .mergePDF].contains(id) }
        let kinds = exts.map { caps.kind($0) }
        switch id.kind {
        case .convert(let p):
            for (ext, kind) in zip(exts, kinds) {
                guard p.takes(ext, kind: kind) else { return false }
                if let t = p.fixedTarget {
                    guard caps.reaches(ext, t) else { return false }
                } else if case .pick(let tk, let allowSame, _) = p.target {
                    let offered = caps.targets(ext).contains { tk.isEmpty || caps.kind($0.to).map(tk.contains) == true }
                    let flags = Set(p.controls.flatMap(\.flags) + (p.alwaysStrip ? ["--strip-metadata"] : []))
                    let keeps = allowSame && !caps.writeTuning(ext).isDisjoint(with: flags)
                    guard offered || keeps else { return false }
                }
            }
            return true
        case .trim:
            let want = id == .trimAudio ? "audio" : "video"
            return exts.count == 1 && kinds[0] == want
        case .mergePDF:
            return exts.allSatisfy { $0 == "pdf" }
        case .photosToPDF:
            return exts.allSatisfy(photos.accepts.contains)
        case .split:
            return exts == ["pdf"]
        case .ascii:
            return exts.count == 1 && (kinds[0] == "image" || kinds[0] == "video")
        case .data:
            return exts.count == 1 && ToolID.dataExts.contains(exts[0])
        case .scan, .qr, .backends, .formats, .changelog:
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
        let exts = urls.map { $0.pathExtension.lowercased() }
        await caps.ensure(Set(exts))
        let kinds = Set(exts.compactMap(caps.kind))
        dropped = urls
        defer { page = .home }
        if folder {
            // A folder: see what's in it, convert all of it, or join its PDFs.
            let hasPDFs = urls.contains { url in
                ((try? fm.contentsOfDirectory(atPath: url.path)) ?? []).contains { $0.lowercased().hasSuffix(".pdf") }
            }
            suggestions = [.scan, .convert].filter(available) + (hasPDFs && available(.mergePDF) ? [.mergePDF] : [])
            return
        }
        let fitting = ToolID.allCases.filter { $0 != .convert && fits($0, exts: exts) }
        let own = fitting.filter { $0.category.kind.map(kinds.contains) ?? false }
        suggestions = own + fitting.filter { !own.contains($0) } + (available(.convert) ? [.convert] : [])
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
        if case .tool(let id) = page {
            let exts = urls.map { $0.pathExtension.lowercased() }
            if id == .convert || fits(id, exts: exts) {
                open(id, with: urls)
                return
            }
        }
        Task { await suggest(for: urls) }
    }

    /// Shows a tool with these files in it.
    @MainActor
    func open(_ id: ToolID, with urls: [URL]) {
        page = .tool(id)
        guard let first = urls.first else { return }
        switch id.kind {
        case .convert: jobs[id]?.add(urls)
        case .trim: (id == .trimAudio ? trimAudio : trimVideo).load(first)
        case .mergePDF: merge.add(urls)
        case .photosToPDF: photos.add(urls)
        case .split: split.load(first)
        case .ascii: ascii.input = first
        case .data: data.load(first)
        case .scan: Task { await scan.scan(first) }
        case .qr, .backends, .formats, .changelog: break
        }
        if urls == dropped { clearDropped() }
    }

    /// Carries a focused tool's files and settings into Convert Anything.
    @MainActor
    func moreOptions(from job: ConvertJob) {
        guard let all = jobs[.convert], job !== all else { return }
        job.copySettings(to: all)
        page = .tool(.convert)
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
            return err.message + " Backends, at the bottom of the sidebar, shows how to install it."
        default:
            return err.message
                .replacingOccurrences(of: "; run `conv capabilities` to list supported pairs", with: "")
        }
    }
}
