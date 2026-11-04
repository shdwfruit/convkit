import SwiftUI
import UniformTypeIdentifiers

/// The window: a bar of sections under the toolbar, each a dropdown of its tools,
/// and the tool (or Home) below it. Search sits in the toolbar and its matches drop
/// down over whatever is showing.
struct RootView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
        detail
            .id("\(model.page)\(model.editorFocus?.rawValue ?? "")")
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .overlay(alignment: .topTrailing) {
                if model.isSearching { SearchResults() }
            }
            .safeAreaInset(edge: .top, spacing: 0) {
                VStack(spacing: 0) {
                    TopBar()
                    if let chain = model.chain, case .tool(let id) = model.page, chain.steps.contains(id) {
                        ChainBar(chain: chain, current: id)
                    }
                }
            }
            .searchable(text: $model.search, placement: .toolbar, prompt: "Search tools")
            .onSubmit(of: .search) {
                if let first = model.searchResults.first { model.show(first) }
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

    @ViewBuilder
    private var detail: some View {
        switch model.page {
        case .home:
            HomeView()
        case .tool(let id):
            switch id.kind {
            case .convert(let preset):
                if let job = model.jobs[id] {
                    if let media = preset.editor {
                        EditorView(job: job, media: media, focus: model.editorFocus)
                    } else {
                        ConvertView(job: job)
                    }
                }
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

/// Under the toolbar: Home, then each section as a dropdown of its tools. The section
/// of the tool showing is marked.
struct TopBar: View {
    @Environment(AppModel.self) private var model
    @State private var width: CGFloat = 1200

    /// How much of each button fits: names and icons, then names, then icons.
    enum Fit { case full, names, icons }

    var body: some View {
        let fit: Fit = width >= 900 ? .full : width >= 720 ? .names : .icons
        HStack(spacing: 2) {
            BarButton(title: "Home", symbol: "square.grid.2x2", color: .gray,
                      selected: model.page == .home, fit: fit) {
                model.menuOpen = nil
                model.page = .home
            }
            .help("Every tool, and what fits a file you drop (⌘0)")
            Divider().frame(height: 16).padding(.horizontal, 4)
            ForEach(Category.allCases) { category in
                let tools = model.menuTools(in: category)
                if !tools.isEmpty {
                    BarButton(title: category.title, symbol: category.symbol, color: category.color,
                              selected: showing == category, open: model.menuOpen == category, dropdown: true, fit: fit) {
                        model.menuOpen = model.menuOpen == category ? nil : category
                    }
                    .help(tools.map(\.title).joined(separator: ", "))
                    .popover(isPresented: Binding(
                        get: { model.menuOpen == category },
                        set: { if !$0, model.menuOpen == category { model.menuOpen = nil } }
                    ), arrowEdge: .bottom) {
                        CategoryMenu(category: category, tools: tools) { tool in
                            model.menuOpen = nil
                            model.show(tool)
                        }
                    }
                }
            }
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 5)
        .frame(maxWidth: .infinity)
        .background {
            GeometryReader { g in
                Color.clear
                    .onAppear { width = g.size.width }
                    .onChange(of: g.size.width) { _, w in width = w }
            }
        }
        .background(.bar)
        .overlay(alignment: .bottom) { Divider() }
    }

    /// The section of the tool showing, if a tool is.
    private var showing: Category? {
        if case .tool(let id) = model.page { return id.category }
        return nil
    }
}

/// One button in the top bar.
private struct BarButton: View {
    let title: String
    let symbol: String
    let color: Color
    var selected = false
    var open = false
    var dropdown = false
    var fit: TopBar.Fit = .full
    let action: () -> Void
    @State private var hover = false

    var body: some View {
        Button(action: action) {
            HStack(spacing: 5) {
                if fit != .names {
                    Image(systemName: symbol)
                        .foregroundStyle(color)
                        .frame(width: 18)
                }
                if fit != .icons {
                    Text(title)
                        .fontWeight(selected ? .semibold : .regular)
                        .foregroundStyle(.primary)
                }
                if dropdown {
                    Image(systemName: "chevron.down")
                        .font(.system(size: 8, weight: .bold))
                        .foregroundStyle(.tertiary)
                }
            }
            .padding(.horizontal, 9)
            .padding(.vertical, 5)
            .background {
                RoundedRectangle(cornerRadius: 7, style: .continuous)
                    .fill(Color.primary.opacity(selected ? 0.1 : open || hover ? 0.06 : 0))
            }
            .contentShape(RoundedRectangle(cornerRadius: 7, style: .continuous))
        }
        .buttonStyle(.plain)
        .onHover { hover = $0 }
        .accessibilityLabel(title)
        .accessibilityAddTraits(selected ? .isSelected : [])
    }
}

/// A section's dropdown: every tool in it with what it does, in two columns when
/// there are many.
struct CategoryMenu: View {
    @Environment(AppModel.self) private var model
    let category: Category
    let tools: [ToolID]
    let pick: (ToolID) -> Void

    var body: some View {
        let columns = tools.count > 5 ? 2 : 1
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 7) {
                Image(systemName: category.symbol).foregroundStyle(category.color)
                Text(category.title).font(.headline)
            }
            .padding(.horizontal, 8)
            .padding(.top, 2)
            LazyVGrid(columns: Array(repeating: GridItem(.fixed(270), spacing: 4, alignment: .top), count: columns),
                      alignment: .leading, spacing: 2) {
                ForEach(tools) { tool in
                    ToolTile(tool: tool, current: model.page == .tool(tool)) { pick(tool) }
                }
            }
        }
        .padding(10)
    }
}

/// What the search matches, dropped down under the search field over whatever is
/// showing. Return opens the first; clicking outside or Escape clears the search.
struct SearchResults: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        let results = model.searchResults
        ZStack(alignment: .topTrailing) {
            Color.black.opacity(0.08)
                .contentShape(Rectangle())
                .onTapGesture { model.search = "" }
            VStack(alignment: .leading, spacing: 0) {
                if results.isEmpty {
                    Text("No tool matches “\(model.search.trimmingCharacters(in: .whitespaces))”.")
                        .foregroundStyle(.secondary)
                        .padding(14)
                } else {
                    ScrollView {
                        VStack(alignment: .leading, spacing: 0) {
                            ForEach(Array(results.enumerated()), id: \.element) { i, tool in
                                SearchRow(tool: tool, first: i == 0) { model.show(tool) }
                            }
                        }
                        .padding(6)
                    }
                    .frame(maxHeight: 440)
                    .fixedSize(horizontal: false, vertical: true)
                }
            }
            .frame(width: 380, alignment: .leading)
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
            .overlay {
                RoundedRectangle(cornerRadius: 10, style: .continuous)
                    .strokeBorder(Color.primary.opacity(0.12))
            }
            .shadow(color: .black.opacity(0.18), radius: 14, y: 6)
            .padding(.top, 6)
            .padding(.trailing, 12)
        }
    }
}

/// A search match: icon, name and section, with what it does underneath.
private struct SearchRow: View {
    let tool: ToolID
    var first = false
    let action: () -> Void
    @State private var hover = false

    var body: some View {
        Button(action: action) {
            HStack(alignment: .top, spacing: 10) {
                ToolIcon(tool: tool, size: 26)
                VStack(alignment: .leading, spacing: 1) {
                    HStack(spacing: 6) {
                        Text(tool.title).font(.body.weight(.medium)).foregroundStyle(.primary)
                        Text(tool.category.title).foregroundStyle(.secondary)
                        Spacer(minLength: 4)
                        if first {
                            Image(systemName: "return")
                                .font(.caption)
                                .foregroundStyle(.tertiary)
                                .help("Return opens this one")
                        }
                    }
                    Text(tool.subtitle)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 6)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background {
                RoundedRectangle(cornerRadius: 7, style: .continuous)
                    .fill(hover ? Color.accentColor.opacity(0.16) : first ? Color.primary.opacity(0.06) : .clear)
            }
            .contentShape(RoundedRectangle(cornerRadius: 7, style: .continuous))
        }
        .buttonStyle(.plain)
        .onHover { hover = $0 }
    }
}

/// Save-panel helper shared by the tools that make one file.
@MainActor
func askWhereToSave(name: String, types: [String]) -> URL? {
    let panel = NSSavePanel()
    panel.nameFieldStringValue = name
    if let folder = SaveDefaults.folder { panel.directoryURL = folder }
    panel.allowedContentTypes = types.compactMap { UTType(filenameExtension: $0) }
    panel.allowsOtherFileTypes = false
    panel.isExtensionHidden = false
    return panel.runModal() == .OK ? panel.url : nil
}
