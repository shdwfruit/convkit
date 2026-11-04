import SwiftUI
import UniformTypeIdentifiers

/// The window: a searchable sidebar of every tool, grouped by what it works on, and
/// the tool (or Home) beside it.
struct RootView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
        NavigationSplitView {
            List(selection: Binding<Page?>(
                get: { model.page },
                set: { if let p = $0 { model.page = p } }
            )) {
                Label {
                    Text("Home")
                } icon: {
                    TileIcon(symbol: "square.grid.2x2", color: .gray, size: 20)
                }
                .tag(Page.home)
                ForEach(Category.allCases) { category in
                    let tools = model.tools(in: category)
                    if !tools.isEmpty {
                        Section(isExpanded: expanded(category)) {
                            ForEach(tools) { tool in
                                Label {
                                    Text(tool.title)
                                } icon: {
                                    ToolIcon(tool: tool, size: 20)
                                }
                                .help(tool.subtitle)
                                .tag(Page.tool(tool))
                            }
                        } header: {
                            Text(category.title)
                        }
                    }
                }
                if model.isSearching && model.searchResults.isEmpty {
                    Text("No tool matches “\(model.search)”.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                }
            }
            .searchable(text: $model.search, placement: .sidebar, prompt: "Search tools")
            .onSubmit(of: .search) {
                if let first = model.searchResults.first { model.page = .tool(first) }
            }
            .navigationSplitViewColumnWidth(min: 200, ideal: 220, max: 280)
        } detail: {
            detail
                .id(model.page)
        }
        .alert("Nothing to paste", isPresented: Binding(
            get: { model.pasteProblem != nil },
            set: { if !$0 { model.pasteProblem = nil } }
        )) {
            Button("OK") { model.pasteProblem = nil }
        } message: {
            Text(model.pasteProblem ?? "")
        }
    }

    /// Searching opens every section; otherwise the user's folding is kept.
    private func expanded(_ category: Category) -> Binding<Bool> {
        Binding(
            get: { model.isSearching || !model.collapsed.contains(category) },
            set: { open in
                if open { model.collapsed.remove(category) } else { model.collapsed.insert(category) }
            }
        )
    }

    @ViewBuilder
    private var detail: some View {
        switch model.page {
        case .home:
            HomeView()
        case .tool(let id):
            switch id.kind {
            case .convert:
                if let job = model.jobs[id] { ConvertView(job: job) }
            case .trim:
                TrimView(trim: id == .trimAudio ? model.trimAudio : model.trimVideo, tool: id,
                         paste: model.pasteURLs)
            case .merge:
                MergeView(merge: model.mergeModel(id), paste: model.pasteURLs)
            case .split:
                SplitView(split: model.split, paste: model.pasteURLs)
            case .pages:
                PagesView(pages: model.pages, paste: model.pasteURLs)
            case .quick:
                if let q = model.quick[id] { QuickView(quick: q) }
            case .speak:
                SpeakView(speak: model.speak)
            case .beautify:
                BeautifyView(beautify: model.beautify, paste: model.pasteURLs)
            case .colors:
                ColorsView(colors: model.colors, paste: model.pasteURLs)
            case .wifi:
                WifiView(wifi: model.wifi)
            case .makeArchive:
                MakeArchiveView(archive: model.makeArchive, paste: model.pasteURLs)
            case .openArchive:
                OpenArchiveView(archive: model.openArchive, paste: model.pasteURLs)
            case .scan:
                ScanView(scan: model.scan) { urls in model.open(.convert, with: urls) }
            case .qr:
                QRView(qr: model.qr)
            case .ascii:
                AsciiView(ascii: model.ascii, paste: model.pasteURLs)
            case .data:
                DataView(data: model.data)
            case .backends:
                BackendsView(backends: model.backends)
            case .formats:
                FormatsView(formats: model.formats)
            case .changelog:
                ChangelogView(changelog: model.changelog)
            }
        }
    }
}

/// Save-panel helper shared by the tools that make one file.
@MainActor
func askWhereToSave(name: String, types: [String]) -> URL? {
    let panel = NSSavePanel()
    panel.nameFieldStringValue = name
    panel.allowedContentTypes = types.compactMap { UTType(filenameExtension: $0) }
    panel.allowsOtherFileTypes = false
    panel.isExtensionHidden = false
    return panel.runModal() == .OK ? panel.url : nil
}
