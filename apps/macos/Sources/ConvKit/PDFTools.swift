import Observation
import PDFKit
import SwiftUI
import UniformTypeIdentifiers

/// The plan or result of `conv merge` / `conv split`, as their --json reports it.
struct PDFResponse: Decodable {
    struct Output: Decodable, Hashable {
        var path: String
        var pages: [Int]?
    }
    struct Entry: Decodable {
        var ok: Bool
        var output: String?
        var outputs: [Output]?
        var pages: Int?
        var warnings: [String]?
        var notes: [String]?
        var error: ConvError?
    }
    var ok: Bool
    var plans: [Entry]?
    var results: [Entry]?
    var error: ConvError?

    var entry: Entry? { (plans ?? results)?.first }
    var problem: String? { (entry?.error ?? error).map(AppModel.friendly) }
}

private func pageCount(_ url: URL) -> Int? { PDFDocument(url: url)?.pageCount }

// MARK: - Merge

/// Joins files into one in the order shown: PDFs (and pictures among them), videos or
/// sound through `conv merge`, and pictures alone through plain `conv a.jpg b.png out.pdf`.
@Observable
final class MergeModel {
    enum Mode { case pdfs, photos, videos, audio }

    struct Item: Identifiable, Hashable {
        let id = UUID()
        let url: URL
        let pages: Int?
    }

    let mode: Mode
    var conv: URL?
    /// Extensions taken, from conv's formats once it's connected.
    var accepts: Set<String>
    /// What a join can be saved as (videos and sound).
    var outputs: [String] = []
    var outputExt = ""
    var items: [Item] = [] {
        didSet { if oldValue.first?.url != items.first?.url { pickOutput() } }
    }
    private(set) var skipped: [String] = []
    private(set) var warnings: [String] = []
    private(set) var problem: String?
    private(set) var result: URL?
    private(set) var working = false

    init(mode: Mode) {
        self.mode = mode
        self.accepts = mode == .pdfs ? ["pdf"] : []
        switch mode {
        case .videos: outputs = ["mp4", "mov", "mkv", "webm"]
        case .audio: outputs = ["m4a", "mp3", "wav", "flac", "aiff", "opus", "ogg"]
        case .pdfs, .photos: outputs = ["pdf"]
        }
        outputExt = outputs[0]
    }

    var tool: ToolID {
        switch mode {
        case .pdfs: .mergePDF
        case .photos: .photosToPDF
        case .videos: .joinVideos
        case .audio: .joinAudio
        }
    }
    var title: String { tool.title }
    var noun: String {
        switch mode {
        case .pdfs: "PDFs"
        case .photos: "pictures"
        case .videos: "videos"
        case .audio: "audio files"
        }
    }
    private var minimum: Int { mode == .photos ? 1 : 2 }

    /// The first file's own format when a join can be saved as it.
    private func pickOutput() {
        guard mode == .videos || mode == .audio, let first = items.first else { return }
        let ext = fileExt(first.url)
        if outputs.contains(ext) { outputExt = ext }
    }

    @MainActor
    func add(_ urls: [URL]) {
        skipped = []
        for url in urls {
            var isDir: ObjCBool = false
            FileManager.default.fileExists(atPath: url.path, isDirectory: &isDir)
            let files = isDir.boolValue
                ? ((try? FileManager.default.contentsOfDirectory(at: url, includingPropertiesForKeys: nil,
                                                                 options: [.skipsHiddenFiles])) ?? [])
                    .sorted { $0.lastPathComponent.localizedStandardCompare($1.lastPathComponent) == .orderedAscending }
                : [url]
            for file in files {
                let ext = fileExt(file)
                if accepts.contains(ext) {
                    let pages: Int? = switch mode {
                    case .pdfs: ext == "pdf" ? pageCount(file) : 1
                    case .photos: 1
                    case .videos, .audio: nil
                    }
                    items.append(Item(url: file, pages: pages))
                } else if !isDir.boolValue {
                    skipped.append(file.lastPathComponent)
                }
            }
        }
        result = nil
        Task { await check() }
    }

    var totalPages: Int { items.compactMap(\.pages).reduce(0, +) }
    var ready: Bool { items.count >= minimum && problem == nil && !working }

    /// Dry run with a stand-in output, for conv's warnings (bookmarks, protection, size).
    @MainActor
    func check() async {
        guard let conv, items.count >= minimum else {
            warnings = []
            problem = items.count == 1 && mode != .photos ? "Add at least one more." : nil
            return
        }
        let stand = FileManager.default.temporaryDirectory.appendingPathComponent("convkit-merge-check.\(outputExt)")
        guard let out = try? await Conv.run(conv, command + ["--dry-run", "--json", "--no-install"]
            + items.map(\.url.path) + [stand.path]) else { return }
        let name = { (s: String) in s.replacingOccurrences(of: stand.lastPathComponent, with: "the new file") }
        switch mode {
        case .pdfs, .videos, .audio:
            guard let resp = out.decode(PDFResponse.self) else {
                problem = Conv.stderrMessage(out.stderr)
                return
            }
            problem = resp.problem
            warnings = ((resp.entry?.warnings ?? []) + (resp.entry?.notes ?? [])).map(name)
        case .photos:
            guard let resp = out.decode(DryRunResponse.self) else {
                problem = Conv.stderrMessage(out.stderr)
                return
            }
            problem = (resp.plans?.first?.error ?? resp.error).map(AppModel.friendly)
            warnings = (resp.plans?.first?.plan?.warnings ?? []).map(name)
        }
    }

    private var command: [String] { mode == .photos ? [] : ["merge"] }

    /// The name a join gets, from the first file.
    private var outputName: String {
        let first = items.first?.url.deletingPathExtension().lastPathComponent ?? "merged"
        return first + (mode == .photos ? "" : "-merged") + "." + outputExt
    }

    /// Joins into a chain's scratch folder and gives back the file, for the next step.
    @MainActor
    func mergeForChain(into folder: URL) async -> URL? {
        guard let conv, items.count >= minimum else { return nil }
        let url = folder.appendingPathComponent(outputName)
        working = true
        defer { working = false }
        guard let out = try? await Conv.run(conv, command + ["--json", "--no-install", "-y"] + items.map(\.url.path) + [url.path])
        else { return nil }
        if mode == .photos {
            let r = out.decode(RunResponse.self)
            if let e = r?.results?.first?.error ?? r?.error { problem = AppModel.friendly(e); return nil }
        } else if let p = out.decode(PDFResponse.self)?.problem {
            problem = p
            return nil
        }
        return FileManager.default.fileExists(atPath: url.path) ? url : nil
    }

    @MainActor
    func merge() {
        guard let conv, items.count >= minimum else { return }
        guard let url = askWhereToSave(name: outputName, types: [outputExt]) else { return }
        // The save panel has already asked about replacing, so -y is the user's answer.
        let args = command + ["--json", "--no-install", "-y"] + items.map(\.url.path) + [url.path]
        Task { @MainActor in
            working = true
            defer { working = false }
            guard let out = try? await Conv.run(conv, args) else {
                problem = "conv didn't answer."
                return
            }
            switch mode {
            case .pdfs, .videos, .audio:
                guard let resp = out.decode(PDFResponse.self) else { problem = Conv.stderrMessage(out.stderr); return }
                if let p = resp.problem {
                    problem = p
                } else {
                    result = resp.entry?.output.map(URL.init(fileURLWithPath:)) ?? url
                    warnings = (resp.entry?.warnings ?? []) + (resp.entry?.notes ?? [])
                }
            case .photos:
                guard let resp = out.decode(RunResponse.self) else { problem = Conv.stderrMessage(out.stderr); return }
                let r = resp.results?.first
                if let r, r.ok {
                    result = r.output.map(URL.init(fileURLWithPath:)) ?? url
                    warnings = (r.warnings ?? []) + (r.notes ?? [])
                } else {
                    problem = (r?.error ?? resp.error).map(AppModel.friendly) ?? "conv didn't say what went wrong."
                }
            }
        }
    }
}

struct MergeView: View {
    @Environment(AppModel.self) private var model
    @Bindable var merge: MergeModel
    let paste: () async -> [URL]

    private var dropTitle: String {
        switch merge.mode {
        case .pdfs: "Drop PDFs to join"
        case .photos: "Drop pictures to put in a PDF"
        case .videos: "Drop videos to join"
        case .audio: "Drop recordings to join"
        }
    }

    private var dropDetail: String {
        switch merge.mode {
        case .pdfs: "Joined in the order shown. Text stays selectable and links keep working. A picture among them gets a page the size of the one beside it."
        case .photos: "A page each, in the order shown. A JPEG goes in as it is, without being re-encoded."
        case .videos: "One after another, in the order shown. Clips that match are joined without re-encoding; the rest are fitted to the first one's size, never stretched."
        case .audio: "One after another, in the order shown. A video's sound can go in too."
        }
    }

    var body: some View {
        VStack(spacing: 0) {
            if merge.items.isEmpty {
                VStack(spacing: 14) {
                    ToolIcon(tool: merge.tool, size: 52)
                    Text(dropTitle).font(.title2.weight(.semibold))
                    Text(dropDetail)
                        .foregroundStyle(.secondary)
                        .multilineTextAlignment(.center)
                        .frame(maxWidth: 420)
                    Button("Choose…") { choose() }.controlSize(.large)
                    if !merge.skipped.isEmpty { skippedNote }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                List {
                    ForEach(merge.items) { item in
                        HStack {
                            Image(systemName: "line.3.horizontal").foregroundStyle(.tertiary)
                            Image(nsImage: NSWorkspace.shared.icon(forFile: item.url.path))
                                .resizable().frame(width: 24, height: 24)
                            Text(item.url.lastPathComponent).lineLimit(1).truncationMode(.middle)
                            Spacer()
                            if merge.mode == .pdfs, let p = item.pages {
                                Text(p == 1 ? "1 page" : "\(p) pages").foregroundStyle(.secondary)
                            }
                            Button {
                                merge.items.removeAll { $0.id == item.id }
                                Task { await merge.check() }
                            } label: {
                                Image(systemName: "xmark.circle.fill").foregroundStyle(.tertiary)
                            }
                            .buttonStyle(.plain)
                        }
                    }
                    .onMove { from, to in
                        merge.items.move(fromOffsets: from, toOffset: to)
                        Task { await merge.check() }
                    }
                }
                footer
            }
        }
        .dropDestination(for: URL.self) { urls, _ in
            merge.add(urls)
            return true
        }
        .navigationTitle(merge.title)
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                Button { choose() } label: { Label("Add \(merge.noun)", systemImage: "plus") }
                Button { Task { merge.add(await paste()) } } label: {
                    Label("Paste", systemImage: "doc.on.clipboard")
                }
                .help(merge.mode == .photos ? "Add pictures copied in Finder, or a copied picture" : "Add files copied in Finder")
                ThenMenu(tools: model.nextTools(after: merge.tool, making: [URL(fileURLWithPath: "/tmp/joined." + merge.outputExt)]),
                         disabled: !merge.ready) { next in
                    guard let origin = merge.items.first?.url else { return }
                    Task {
                        await model.chain(from: merge.tool, origin: origin, fromChain: model.chain?.steps.last == merge.tool,
                                          make: { await merge.mergeForChain(into: $0) }, to: next)
                    }
                }
                Button(merge.mode == .photos ? "Make PDF…" : merge.mode == .pdfs ? "Merge…" : "Join…") { merge.merge() }
                    .buttonStyle(.borderedProminent)
                    .disabled(!merge.ready)
            }
        }
        .onChange(of: merge.outputExt) { Task { await merge.check() } }
    }

    private var countLine: String {
        let n = merge.items.count
        switch merge.mode {
        case .pdfs: return "\(n) files, \(merge.totalPages) pages. Drag to reorder."
        case .photos: return "\(n == 1 ? "1 page" : "\(n) pages"). Drag to reorder."
        case .videos, .audio: return "\(n) \(n == 1 ? "file" : "files"). Drag to reorder."
        }
    }

    private var footer: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text(countLine).font(.callout).foregroundStyle(.secondary)
                Spacer()
                if merge.outputs.count > 1 {
                    Picker("Save as", selection: $merge.outputExt) {
                        ForEach(merge.outputs, id: \.self) { Text($0.uppercased()).tag($0) }
                    }
                    .fixedSize()
                }
            }
            if !merge.skipped.isEmpty { skippedNote }
            if let p = merge.problem {
                Label(p, systemImage: "exclamationmark.triangle").foregroundStyle(.orange)
            }
            ForEach(merge.warnings, id: \.self) {
                Text($0).font(.caption).foregroundStyle(.orange)
            }
            if merge.working {
                HStack(spacing: 6) {
                    ProgressView().controlSize(.small)
                    Text(merge.mode == .videos ? "Joining. Clips that don't match are encoded once, which takes a while." : "Working…")
                        .font(.callout).foregroundStyle(.secondary)
                }
            }
            if let url = merge.result {
                Button("Saved \(url.lastPathComponent). Show in Finder") {
                    NSWorkspace.shared.activateFileViewerSelecting([url])
                }
                .buttonStyle(.link)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(12)
        .background(.bar)
    }

    private var skippedNote: some View {
        let what = switch merge.mode {
        case .pdfs: "a PDF or a picture"
        case .photos: "a picture conv puts in a PDF"
        case .videos: "a video"
        case .audio: "audio or video"
        }
        return Label("Left out, not \(what): \(merge.skipped.joined(separator: ", "))", systemImage: "nosign")
            .font(.caption)
            .foregroundStyle(.secondary)
    }

    private func choose() {
        let panel = NSOpenPanel()
        panel.allowsMultipleSelection = true
        panel.allowedContentTypes = merge.accepts.compactMap { UTType(filenameExtension: $0) }
        if panel.runModal() == .OK { merge.add(panel.urls) }
    }
}

// MARK: - Split

@Observable
final class SplitModel {
    var conv: URL?
    private(set) var file: URL?
    private(set) var pages: Int?
    /// Empty means one file a page.
    var ranges = ""
    var outputFolder: URL?

    private(set) var planned: [PDFResponse.Output] = []
    private(set) var warnings: [String] = []
    private(set) var problem: String?
    private(set) var written: [URL] = []
    private(set) var exists = false
    private(set) var working = false
    private var task: Task<Void, Never>?

    @MainActor
    func load(_ url: URL) {
        file = url
        pages = pageCount(url)
        written = []
        changed()
    }

    private func args() -> [String]? {
        guard let file else { return nil }
        var a = [file.path]
        a += ranges.split(whereSeparator: { $0 == "," || $0 == " " }).map(String.init)
        if let dir = outputFolder { a += ["-o", dir.path] }
        return a
    }

    func changed() {
        task?.cancel()
        exists = false
        guard let conv, let a = args() else { return }
        task = Task { @MainActor in
            try? await Task.sleep(for: .milliseconds(200))
            guard !Task.isCancelled,
                  let out = try? await Conv.run(conv, ["split", "--dry-run", "--json", "--no-install"] + a),
                  !Task.isCancelled, let resp = out.decode(PDFResponse.self)
            else { return }
            problem = resp.problem
            planned = resp.entry?.outputs ?? []
            warnings = (resp.entry?.warnings ?? []) + (resp.entry?.notes ?? [])
        }
    }

    @MainActor
    func split(replace: Bool = false) {
        guard let conv, let a = args() else { return }
        // A preview still on its way would clear what this run reports.
        task?.cancel()
        task = Task { @MainActor in
            working = true
            defer { working = false }
            guard let out = try? await Conv.run(conv, ["split", "--json", "--no-install"] + (replace ? ["-y"] : []) + a),
                  let resp = out.decode(PDFResponse.self)
            else {
                problem = "conv didn't answer."
                return
            }
            if let err = resp.entry?.error ?? resp.error, err.code == "output_exists" {
                exists = true
                problem = AppModel.friendly(err)
                return
            }
            exists = false
            problem = resp.problem
            written = (resp.entry?.outputs ?? []).map { URL(fileURLWithPath: $0.path) }
            warnings = (resp.entry?.warnings ?? []) + (resp.entry?.notes ?? [])
        }
    }
}

struct SplitView: View {
    @Bindable var split: SplitModel
    let paste: () async -> [URL]

    var body: some View {
        Group {
            if let file = split.file {
                HSplitView {
                    preview(file)
                        .frame(minWidth: 300, maxWidth: .infinity, maxHeight: .infinity)
                    controls
                        .frame(minWidth: 300, idealWidth: 340, maxWidth: 420)
                }
            } else {
                VStack(spacing: 14) {
                    ToolIcon(tool: .splitPDF, size: 52)
                    Text("Drop a PDF to split").font(.title2.weight(.semibold))
                    Text("One file a page, or one file for each range you type.")
                        .foregroundStyle(.secondary)
                    Button("Choose…") { choose() }.controlSize(.large)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .dropDestination(for: URL.self) { urls, _ in
            guard let first = urls.first, first.pathExtension.lowercased() == "pdf" else { return false }
            split.load(first)
            return true
        }
        .navigationTitle("Split PDF")
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                Button { choose() } label: { Label("Open", systemImage: "folder") }
                Button { Task { if let first = await paste().first(where: { $0.pathExtension.lowercased() == "pdf" }) { split.load(first) } } } label: {
                    Label("Paste", systemImage: "doc.on.clipboard")
                }
                .help("Split a PDF copied in Finder")
                Button(split.planned.count > 1 ? "Split into \(split.planned.count) Files" : "Split") { split.split() }
                    .buttonStyle(.borderedProminent)
                    .disabled(split.file == nil || split.planned.isEmpty || split.working)
            }
        }
        .onChange(of: split.ranges) { split.changed() }
        .onChange(of: split.outputFolder) { split.changed() }
    }

    private func preview(_ file: URL) -> some View {
        PDFThumbnails(url: file)
    }

    private var controls: some View {
        Form {
            Section {
                LabeledContent("File", value: split.file?.lastPathComponent ?? "")
                if let p = split.pages { LabeledContent("Pages", value: "\(p)") }
            }
            Section {
                TextField("Pages for each file", text: $split.ranges, prompt: Text("Every page on its own"))
            } header: {
                Text("Ranges")
            } footer: {
                Text("Like 1-3, 5, 11-z. z is the last page; 5-1 writes them in reverse.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Section("Save in") {
                Picker("Save in", selection: Binding(
                    get: { split.outputFolder == nil ? 0 : 1 },
                    set: { if $0 == 0 { split.outputFolder = nil } else { chooseFolder() } }
                )) {
                    Text("Next to the PDF").tag(0)
                    Text(split.outputFolder?.lastPathComponent ?? "Choose…").tag(1)
                }
                .labelsHidden()
                .pickerStyle(.radioGroup)
            }
            Section(split.written.isEmpty ? "Will write" : "Wrote") {
                if let p = split.problem {
                    Label(p, systemImage: "exclamationmark.triangle").foregroundStyle(.orange)
                    if split.exists {
                        Button("Replace Them") { split.split(replace: true) }
                    }
                }
                ForEach(split.warnings, id: \.self) {
                    Text($0).font(.caption).foregroundStyle(.orange)
                }
                if split.written.isEmpty {
                    ForEach(split.planned.prefix(50), id: \.self) { o in
                        LabeledContent((o.path as NSString).lastPathComponent,
                                       value: Self.describe(o.pages))
                    }
                    if split.planned.count > 50 {
                        Text("and \(split.planned.count - 50) more").foregroundStyle(.secondary)
                    }
                } else {
                    ForEach(split.written, id: \.self) { url in
                        Button(url.lastPathComponent) { NSWorkspace.shared.activateFileViewerSelecting([url]) }
                            .buttonStyle(.link)
                    }
                }
            }
        }
        .formStyle(.grouped)
    }

    static func describe(_ pages: [Int]?) -> String {
        guard let pages, let first = pages.first, let last = pages.last else { return "" }
        if pages.count == 1 { return "page \(first)" }
        return "pages \(first)–\(last)"
    }

    private func choose() {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = [.pdf]
        if panel.runModal() == .OK, let url = panel.url { split.load(url) }
    }

    private func chooseFolder() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.prompt = "Save Here"
        if panel.runModal() == .OK, let url = panel.url { split.outputFolder = url }
    }
}

// MARK: - Pages

/// `conv pages`: keep, reorder, delete and turn a PDF's pages into a new PDF.
@Observable
final class PagesModel {
    var conv: URL?
    private(set) var file: URL?
    private(set) var pages: Int?
    private(set) var locked = false
    /// In the order to write them; empty keeps every page.
    var keep = ""
    var delete = ""
    var turn = ""
    /// Empty turns every page.
    var turnPages = ""
    var password = ""
    /// Where it's written; nil is next to the PDF.
    var outputFolder: URL?

    private(set) var planned: URL?
    private(set) var plannedPages: Int?
    private(set) var warnings: [String] = []
    private(set) var problem: String?
    private(set) var written: URL?
    private(set) var exists = false
    private(set) var working = false
    private var task: Task<Void, Never>?

    @MainActor
    func load(_ url: URL) {
        file = url
        let doc = PDFDocument(url: url)
        pages = doc?.pageCount
        locked = doc?.isLocked ?? false
        written = nil
        changed()
    }

    private var args: [String]? {
        guard let file else { return nil }
        var a = [file.path]
        a += keep.split(whereSeparator: { $0 == " " }).map(String.init)
        let del = delete.replacingOccurrences(of: " ", with: "")
        if !del.isEmpty { a += ["--delete", del] }
        if !turn.isEmpty {
            let which = turnPages.replacingOccurrences(of: " ", with: "")
            a += ["--rotate", which.isEmpty ? turn : "\(turn):\(which)"]
        }
        if let outputFolder { a += ["-o", outputFolder.path] }
        return a
    }

    var nothingToDo: Bool { keep.trimmingCharacters(in: .whitespaces).isEmpty && delete.isEmpty && turn.isEmpty }
    private var env: [String: String] { password.isEmpty ? [:] : ["CONVKIT_PASSWORD": password] }

    func changed() {
        task?.cancel()
        exists = false
        guard let conv, let a = args else { return }
        if nothingToDo {
            planned = nil
            warnings = []
            problem = nil
            return
        }
        task = Task { @MainActor in
            try? await Task.sleep(for: .milliseconds(200))
            guard !Task.isCancelled,
                  let out = try? await Conv.run(conv, ["pages", "--dry-run", "--json", "--no-install"] + a, env: env),
                  !Task.isCancelled else { return }
            guard let resp = out.decode(PDFResponse.self) else {
                problem = Conv.stderrMessage(out.stderr)
                planned = nil
                return
            }
            problem = resp.problem
            planned = resp.entry?.output.map(URL.init(fileURLWithPath:))
            plannedPages = resp.entry?.pages
            warnings = (resp.entry?.warnings ?? []) + (resp.entry?.notes ?? [])
        }
    }

    var ready: Bool { planned != nil && problem == nil && !working }

    /// Writes into a chain's scratch folder and gives back the file, for the next step.
    @MainActor
    func writeForChain(into folder: URL) async -> URL? {
        guard let conv, let file else { return nil }
        let saved = outputFolder
        outputFolder = folder
        defer { outputFolder = saved }
        guard let a = args,
              let out = try? await Conv.run(conv, ["pages", "--json", "--no-install", "-y"] + a, env: env),
              let resp = out.decode(PDFResponse.self) else { return nil }
        if let p = resp.problem { problem = p; return nil }
        _ = file
        return resp.entry?.output.map(URL.init(fileURLWithPath:))
    }

    @MainActor
    func write(replace: Bool = false) {
        guard let conv, let a = args else { return }
        task?.cancel()
        task = Task { @MainActor in
            working = true
            defer { working = false }
            guard let out = try? await Conv.run(conv, ["pages", "--json", "--no-install"] + (replace ? ["-y"] : []) + a, env: env),
                  let resp = out.decode(PDFResponse.self) else {
                problem = "conv didn't answer."
                return
            }
            if let err = resp.entry?.error ?? resp.error {
                exists = err.code == "output_exists"
                problem = AppModel.friendly(err)
                return
            }
            exists = false
            problem = nil
            written = resp.entry?.output.map(URL.init(fileURLWithPath:)) ?? planned
            warnings = (resp.entry?.warnings ?? []) + (resp.entry?.notes ?? [])
        }
    }
}

struct PagesView: View {
    @Environment(AppModel.self) private var model
    @Bindable var pages: PagesModel
    let paste: () async -> [URL]

    var body: some View {
        Group {
            if let file = pages.file {
                HSplitView {
                    PDFThumbnails(url: file)
                        .frame(minWidth: 300, maxWidth: .infinity, maxHeight: .infinity)
                    controls
                        .frame(minWidth: 300, idealWidth: 340, maxWidth: 420)
                }
            } else {
                VStack(spacing: 14) {
                    ToolIcon(tool: .pdfPages, size: 52)
                    Text("Drop a PDF to arrange").font(.title2.weight(.semibold))
                    Text("Keep the pages you list in that order, leave some out, or turn them. Bookmarks, links and metadata stay.")
                        .foregroundStyle(.secondary).multilineTextAlignment(.center).frame(maxWidth: 420)
                    Button("Choose…") { choose() }.controlSize(.large)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .dropDestination(for: URL.self) { urls, _ in
            guard let first = urls.first, first.pathExtension.lowercased() == "pdf" else { return false }
            pages.load(first)
            return true
        }
        .navigationTitle("Arrange Pages")
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                Button { choose() } label: { Label("Open", systemImage: "folder") }
                Button { Task { if let f = await paste().first(where: { $0.pathExtension.lowercased() == "pdf" }) { pages.load(f) } } } label: {
                    Label("Paste", systemImage: "doc.on.clipboard")
                }
                .help("Use a PDF copied in Finder")
                ThenMenu(tools: model.nextTools(after: .pdfPages, making: [URL(fileURLWithPath: "/tmp/pages.pdf")]),
                         disabled: !pages.ready) { next in
                    guard let origin = pages.file else { return }
                    Task {
                        await model.chain(from: .pdfPages, origin: origin, fromChain: model.chain?.steps.last == .pdfPages,
                                          make: { await pages.writeForChain(into: $0) }, to: next)
                    }
                }
                Button("Save") { pages.write() }
                    .buttonStyle(.borderedProminent)
                    .disabled(!pages.ready)
            }
        }
        .onChange(of: [pages.keep, pages.delete, pages.turn, pages.turnPages, pages.password]) { pages.changed() }
    }

    private var controls: some View {
        Form {
            Section {
                LabeledContent("File", value: pages.file?.lastPathComponent ?? "")
                if let p = pages.pages { LabeledContent("Pages", value: "\(p)") }
                if pages.locked {
                    SecureField("Password", text: $pages.password, prompt: Text("It's locked"))
                }
            }
            Section {
                TextField("Keep", text: $pages.keep, prompt: Text("Every page, in order"))
                TextField("Leave out", text: $pages.delete, prompt: Text("None"))
            } header: {
                Text("Pages")
            } footer: {
                Text("Keep lists pages in the order to write them: 3 1-2 5-z. z is the last page, 5-1 is reversed, and a page listed twice appears twice. Leave out takes 4,6-7. Numbers are always the original's.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Section("Turn") {
                Picker("Turn", selection: $pages.turn) {
                    Text("Don't turn").tag("")
                    Text("Right (90° clockwise)").tag("right")
                    Text("Left (90° counterclockwise)").tag("left")
                    Text("Upside down (180°)").tag("180")
                }
                TextField("Which pages", text: $pages.turnPages, prompt: Text("All of them"))
                    .why(pages.turn.isEmpty ? "Choose a turn first." : nil)
            }
            Section(pages.written == nil ? "Will write" : "Wrote") {
                if pages.nothingToDo {
                    Text("Say which pages to keep, leave out or turn.").foregroundStyle(.secondary)
                }
                if let p = pages.problem {
                    Label(p, systemImage: "exclamationmark.triangle").foregroundStyle(.orange)
                    if pages.exists { Button("Replace It") { pages.write(replace: true) } }
                }
                if let w = pages.written {
                    Button(w.lastPathComponent) { NSWorkspace.shared.activateFileViewerSelecting([w]) }.buttonStyle(.link)
                } else if let p = pages.planned {
                    LabeledContent(p.lastPathComponent, value: pages.plannedPages.map { $0 == 1 ? "1 page" : "\($0) pages" } ?? "")
                }
                ForEach(pages.warnings, id: \.self) { Text($0).font(.caption).foregroundStyle(.orange) }
            }
        }
        .formStyle(.grouped)
    }

    private func choose() {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = [.pdf]
        if panel.runModal() == .OK, let url = panel.url { pages.load(url) }
    }
}

/// The PDF's pages, so ranges are easy to pick.
struct PDFThumbnails: NSViewRepresentable {
    let url: URL
    func makeNSView(context: Context) -> PDFView {
        let v = PDFView()
        v.autoScales = true
        v.displayMode = .singlePageContinuous
        v.document = PDFDocument(url: url)
        return v
    }
    func updateNSView(_ v: PDFView, context: Context) {
        if v.document?.documentURL != url { v.document = PDFDocument(url: url) }
    }
}
