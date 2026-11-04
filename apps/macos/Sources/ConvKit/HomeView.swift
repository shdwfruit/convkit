import AppKit
import SwiftUI

/// Every tool at a glance: a drop target that lists the tools fitting a file, the
/// popular ones, the ones used lately, then each section in full. The sidebar's
/// search filters it too.
struct HomeView: View {
    @Environment(AppModel.self) private var model
    @State private var dropHover = false

    var body: some View {
        ScrollView {
            HomeContent(dropHover: dropHover)
        }
        .navigationTitle("ConvKit")
        .dropDestination(for: URL.self) { urls, _ in
            Task { await model.suggest(for: urls) }
            return true
        } isTargeted: { dropHover = $0 }
        .toolbar {
            ToolbarItem(placement: .primaryAction) {
                Button { pickFiles(model) } label: { Label("Open Files", systemImage: "plus") }
                    .help("Choose files and see which tools fit them (⌘O)")
            }
        }
    }
}

/// Home's page, apart from its scroll view so it can be drawn on its own.
struct HomeContent: View {
    @Environment(AppModel.self) private var model
    var dropHover = false

    private let columns = [GridItem(.adaptive(minimum: 200, maximum: 360), spacing: 6, alignment: .top)]

    var body: some View {
            VStack(alignment: .leading, spacing: 30) {
                if model.isSearching {
                    Text("Tools for “\(model.search.trimmingCharacters(in: .whitespaces))”")
                        .font(.title2.weight(.semibold))
                } else if model.dropped.isEmpty {
                    dropPanel
                } else {
                    suggestions
                }

                if model.isSearching {
                    ForEach(Category.allCases) { category in
                        let tools = model.tools(in: category)
                        if !tools.isEmpty {
                            group(title: category.title, symbol: category.symbol, color: category.color,
                                  tools: tools, count: tools.count)
                        }
                    }
                } else {
                    group(title: "Popular", symbol: "star", color: .yellow, tools: model.popular)
                    if !model.recentTools.isEmpty { recent }
                    VStack(alignment: .leading, spacing: 10) {
                        Text("Every tool").font(.title3.weight(.semibold))
                        Masonry(minColumn: 250, spacing: 12) {
                            ForEach(Category.allCases) { category in
                                let tools = model.tools(in: category)
                                if !tools.isEmpty { CategoryBlock(category: category, tools: tools) }
                            }
                        }
                    }
                }
                if model.isSearching && model.searchResults.isEmpty {
                    ContentUnavailableView.search(text: model.search)
                }
            }
            .padding(.horizontal, 32)
            .padding(.vertical, 26)
            .frame(maxWidth: 1180, alignment: .leading)
            .frame(maxWidth: .infinity)
    }

    // MARK: Drop

    private var dropPanel: some View {
        HStack(spacing: 18) {
            Image(systemName: "arrow.down.doc")
                .font(.system(size: 30, weight: .light))
                .foregroundStyle(dropHover ? Color.accentColor : .secondary)
                .frame(width: 44)
            VStack(alignment: .leading, spacing: 4) {
                Text("Drop a file to see what you can do with it")
                    .font(.title3.weight(.semibold))
                Text("Videos, pictures, audio, PDFs, documents or data. Everything runs on this Mac.")
                    .foregroundStyle(.secondary)
            }
            Spacer(minLength: 12)
            Button("Choose Files…") { pickFiles(model) }
                .controlSize(.large)
        }
        .padding(.horizontal, 22)
        .padding(.vertical, 20)
        .background {
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .strokeBorder(dropHover ? Color.accentColor : Color.secondary.opacity(0.35),
                              style: StrokeStyle(lineWidth: dropHover ? 2 : 1.25, dash: [6, 5]))
        }
        .animation(.easeOut(duration: 0.12), value: dropHover)
    }

    private var suggestions: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(alignment: .center, spacing: 12) {
                HStack(spacing: -10) {
                    ForEach(model.dropped.prefix(4), id: \.self) { url in
                        Image(nsImage: NSWorkspace.shared.icon(forFile: url.path))
                            .resizable()
                            .frame(width: 40, height: 40)
                    }
                }
                VStack(alignment: .leading, spacing: 2) {
                    Text(droppedTitle).font(.title3.weight(.semibold)).lineLimit(1).truncationMode(.middle)
                    Text(model.suggestions.isEmpty
                         ? "No tool here takes \(model.dropped.count == 1 ? "this file" : "all of these together")."
                         : "Pick a tool and the \(model.dropped.count == 1 ? "file goes" : "files go") straight in.")
                        .foregroundStyle(.secondary)
                }
                Spacer()
                Button("Clear") { model.clearDropped() }
            }
            LazyVGrid(columns: columns, alignment: .leading, spacing: 6) {
                ForEach(model.suggestions) { tool in
                    ToolTile(tool: tool) { model.open(tool, with: model.dropped) }
                }
            }
        }
        .padding(18)
        .background(.background.secondary, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
    }

    private var droppedTitle: String {
        let first = model.dropped.first?.lastPathComponent ?? ""
        return model.dropped.count == 1 ? "For \(first)" : "For \(first) and \(model.dropped.count - 1) more"
    }

    // MARK: Sections

    private var recent: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                Image(systemName: "clock").foregroundStyle(.secondary)
                Text("Recent").font(.title3.weight(.semibold))
            }
            LazyVGrid(columns: [GridItem(.adaptive(minimum: 190, maximum: 280), spacing: 6)], alignment: .leading, spacing: 2) {
                ForEach(model.recentTools) { tool in
                    ToolRow(tool: tool, showsSection: true) { model.page = .tool(tool) }
                }
            }
        }
    }

    private func group(title: String, symbol: String, color: Color, tools: [ToolID], count: Int? = nil) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                Image(systemName: symbol).foregroundStyle(color)
                Text(title).font(.title3.weight(.semibold))
                if let count { Text("\(count)").foregroundStyle(.tertiary) }
            }
            LazyVGrid(columns: columns, alignment: .leading, spacing: 6) {
                ForEach(tools) { tool in
                    ToolTile(tool: tool) { model.page = .tool(tool) }
                }
            }
        }
    }
}

/// A tool on Home: its icon, name and what it does.
struct ToolTile: View {
    let tool: ToolID
    let action: () -> Void
    @State private var hover = false

    var body: some View {
        Button(action: action) {
            HStack(alignment: .top, spacing: 10) {
                ToolIcon(tool: tool, size: 30)
                VStack(alignment: .leading, spacing: 2) {
                    Text(tool.title)
                        .font(.body.weight(.medium))
                        .foregroundStyle(.primary)
                    Text(tool.subtitle)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                        .multilineTextAlignment(.leading)
                        .fixedSize(horizontal: false, vertical: true)
                }
                Spacer(minLength: 0)
            }
            .padding(8)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background {
                RoundedRectangle(cornerRadius: 8, style: .continuous)
                    .fill(hover ? Color.primary.opacity(0.07) : .clear)
            }
            .contentShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
        }
        .buttonStyle(.plain)
        .onHover { hover = $0 }
        .accessibilityLabel(tool.title)
        .accessibilityHint(tool.subtitle)
    }
}

/// One section on Home: its name and its tools, one line each.
struct CategoryBlock: View {
    @Environment(AppModel.self) private var model
    let category: Category
    let tools: [ToolID]

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 7) {
                Image(systemName: category.symbol).foregroundStyle(category.color)
                Text(category.title).font(.headline)
                Text("\(tools.count)").foregroundStyle(.tertiary)
            }
            .padding(.horizontal, 6)
            .padding(.bottom, 6)
            ForEach(tools) { tool in
                ToolRow(tool: tool) { model.page = .tool(tool) }
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.primary.opacity(0.035), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
    }
}

/// A tool as one line: icon and name, with what it does on hover.
struct ToolRow: View {
    let tool: ToolID
    var showsSection = false
    let action: () -> Void
    @State private var hover = false

    var body: some View {
        Button(action: action) {
            HStack(spacing: 8) {
                ToolIcon(tool: tool, size: 20)
                Text(tool.title).foregroundStyle(.primary)
                if showsSection && [.videoSpeed, .audioSpeed, .stripVideo, .stripAudio].contains(tool) {
                    Text(tool.category.title).foregroundStyle(.secondary)
                }
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 6)
            .padding(.vertical, 4)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background {
                RoundedRectangle(cornerRadius: 6, style: .continuous)
                    .fill(hover ? Color.primary.opacity(0.07) : .clear)
            }
            .contentShape(RoundedRectangle(cornerRadius: 6, style: .continuous))
        }
        .buttonStyle(.plain)
        .onHover { hover = $0 }
        .help(tool.subtitle)
        .accessibilityHint(tool.subtitle)
    }
}

/// Columns of blocks, each block going into the shortest column, so sections of
/// different lengths pack without gaps.
struct Masonry: Layout {
    var minColumn: CGFloat
    var spacing: CGFloat

    private func columns(for width: CGFloat) -> Int {
        max(1, Int((width + spacing) / (minColumn + spacing)))
    }

    private func place(_ subviews: Subviews, width: CGFloat) -> (frames: [CGRect], height: CGFloat) {
        let n = columns(for: width)
        let colWidth = (width - spacing * CGFloat(n - 1)) / CGFloat(n)
        var heights = Array(repeating: CGFloat(0), count: n)
        var frames: [CGRect] = []
        for view in subviews {
            let col = heights.indices.min { heights[$0] < heights[$1] } ?? 0
            let size = view.sizeThatFits(ProposedViewSize(width: colWidth, height: nil))
            let y = heights[col] == 0 ? 0 : heights[col] + spacing
            frames.append(CGRect(x: CGFloat(col) * (colWidth + spacing), y: y, width: colWidth, height: size.height))
            heights[col] = y + size.height
        }
        return (frames, heights.max() ?? 0)
    }

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let width = proposal.width ?? minColumn * 3 + spacing * 2
        return CGSize(width: width, height: place(subviews, width: width).height)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        let (frames, _) = place(subviews, width: bounds.width)
        for (view, frame) in zip(subviews, frames) {
            view.place(at: CGPoint(x: bounds.minX + frame.minX, y: bounds.minY + frame.minY),
                       proposal: ProposedViewSize(width: frame.width, height: frame.height))
        }
    }
}
