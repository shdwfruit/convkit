import Observation
import PDFKit
import SwiftUI

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

@Observable
final class MergeModel {
    struct Item: Identifiable, Hashable {
        let id = UUID()
        let url: URL
        let pages: Int?
    }

    var conv: URL?
    var items: [Item] = []
    private(set) var warnings: [String] = []
    private(set) var problem: String?
    private(set) var result: URL?
    private(set) var working = false

    @MainActor
    func add(_ urls: [URL]) {
        for url in urls {
            var isDir: ObjCBool = false
            FileManager.default.fileExists(atPath: url.path, isDirectory: &isDir)
            let pdfs = isDir.boolValue
                ? ((try? FileManager.default.contentsOfDirectory(at: url, includingPropertiesForKeys: nil)) ?? [])
                    .filter { $0.pathExtension.lowercased() == "pdf" }
                    .sorted { $0.lastPathComponent.localizedStandardCompare($1.lastPathComponent) == .orderedAscending }
                : [url]
            items += pdfs.map { Item(url: $0, pages: pageCount($0)) }
        }
        result = nil
        Task { await check() }
    }

    var totalPages: Int { items.compactMap(\.pages).reduce(0, +) }

    /// Dry run with a stand-in output, for conv's warnings (bookmarks, protection).
    @MainActor
    func check() async {
        guard let conv, items.count >= 2 else {
            warnings = []
            problem = items.count == 1 ? "Add at least one more PDF." : nil
            return
        }
        let stand = FileManager.default.temporaryDirectory.appendingPathComponent("convkit-merge-check.pdf")
        guard let out = try? await Conv.run(conv, ["merge", "--dry-run", "--json", "--no-install"]
            + items.map(\.url.path) + [stand.path]),
            let resp = out.decode(PDFResponse.self) else { return }
        problem = resp.problem
        warnings = (resp.entry?.warnings ?? []).map { $0.replacingOccurrences(of: stand.lastPathComponent, with: "the new PDF") }
            + (resp.entry?.notes ?? [])
    }

    @MainActor
    func merge() {
        guard let conv, items.count >= 2 else { return }
        let first = items[0].url
        guard let url = askWhereToSave(name: first.deletingPathExtension().lastPathComponent + "-merged.pdf",
                                       types: ["pdf"]) else { return }
        // The save panel has already asked about replacing, so -y is the user's answer.
        let args = ["merge", "--json", "--no-install", "-y"] + items.map(\.url.path) + [url.path]
        Task { @MainActor in
            working = true
            defer { working = false }
            guard let out = try? await Conv.run(conv, args), let resp = out.decode(PDFResponse.self) else {
                problem = "conv didn't answer."
                return
            }
            if let p = resp.problem {
                problem = p
            } else {
                result = resp.entry?.output.map(URL.init(fileURLWithPath:)) ?? url
                warnings = (resp.entry?.warnings ?? []) + (resp.entry?.notes ?? [])
            }
        }
    }
}

struct MergeView: View {
    @Bindable var merge: MergeModel
    let paste: () async -> [URL]

    var body: some View {
        VStack(spacing: 0) {
            if merge.items.isEmpty {
                VStack(spacing: 14) {
                    Image(systemName: "doc.on.doc").font(.system(size: 44, weight: .light))
                        .foregroundStyle(.secondary)
                    Text("Drop PDFs to join").font(.title2.weight(.semibold))
                    Text("They're joined in the order shown. Text stays selectable and links keep working.")
                        .foregroundStyle(.secondary)
                    Button("Choose…") { choose() }.controlSize(.large)
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
                            if let p = item.pages {
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
        .navigationTitle("Merge PDFs")
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                Button { choose() } label: { Label("Add PDFs", systemImage: "plus") }
                Button { Task { merge.add(await paste()) } } label: {
                    Label("Paste", systemImage: "doc.on.clipboard")
                }
                .help("Add PDFs copied in Finder")
                Button("Merge…") { merge.merge() }
                    .buttonStyle(.borderedProminent)
                    .disabled(merge.items.count < 2 || merge.problem != nil || merge.working)
            }
        }
    }

    private var footer: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("\(merge.items.count) PDFs, \(merge.totalPages) pages. Drag to reorder.")
                .font(.callout).foregroundStyle(.secondary)
            if let p = merge.problem {
                Label(p, systemImage: "exclamationmark.triangle").foregroundStyle(.orange)
            }
            ForEach(merge.warnings, id: \.self) {
                Text($0).font(.caption).foregroundStyle(.orange)
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

    private func choose() {
        let panel = NSOpenPanel()
        panel.allowsMultipleSelection = true
        panel.allowedContentTypes = [.pdf]
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
                    Image(systemName: "doc.badge.ellipsis").font(.system(size: 44, weight: .light))
                        .foregroundStyle(.secondary)
                    Text("Drop a PDF to split").font(.title2.weight(.semibold))
                    Text("One file a page, or one file for each range you type.")
                        .foregroundStyle(.secondary)
                    Button("Choose…") { choose() }.controlSize(.large)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .dropDestination(for: URL.self) { urls, _ in
            guard let first = urls.first else { return false }
            split.load(first)
            return true
        }
        .navigationTitle("Split PDF")
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                Button { choose() } label: { Label("Open", systemImage: "folder") }
                Button { Task { if let first = await paste().first { split.load(first) } } } label: {
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
                    .font(.system(.body, design: .monospaced))
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
