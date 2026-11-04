import AppKit
import SwiftUI

/// Next to Save: run this step without saving it, and carry what it makes into the
/// next tool. Only the last step saves, next to the file the chain started from.
struct ThenMenu: View {
    let tools: [ToolID]
    var disabled = false
    let action: (ToolID) -> Void

    var body: some View {
        Menu {
            if tools.isEmpty {
                Text("Nothing else takes what this makes")
            }
            ForEach(sections, id: \.0) { title, items in
                Section(title) {
                    ForEach(items) { t in
                        Button { action(t) } label: { Label(t.title, systemImage: t.symbol) }
                    }
                }
            }
        } label: {
            Label("Then", systemImage: "arrow.right.circle")
        }
        .menuIndicator(.visible)
        .disabled(disabled || tools.isEmpty)
        .help("Use the result in another tool without saving it first. Only the last step saves.")
    }

    /// The tools under their section names, in the order given.
    private var sections: [(String, [ToolID])] {
        var out: [(String, [ToolID])] = []
        for t in tools {
            if let i = out.firstIndex(where: { $0.0 == t.category.title }) {
                out[i].1.append(t)
            } else {
                out.append((t.category.title, [t]))
            }
        }
        return out
    }
}

/// For a file that's been saved: open it in another tool.
struct OpenInMenu: View {
    @Environment(AppModel.self) private var model
    let output: URL
    let from: ToolID

    var body: some View {
        let tools = model.nextTools(after: from, making: [output])
        Menu("Open In") {
            ForEach(tools) { t in
                Button { model.open(t, with: [output]) } label: { Label(t.title, systemImage: t.symbol) }
            }
        }
        .menuIndicator(.visible)
        .fixedSize()
        .controlSize(.small)
        .disabled(tools.isEmpty)
    }
}

/// Above a tool that's a step in a chain: where it started, the steps so far, and
/// where the last one saves.
struct ChainBar: View {
    @Environment(AppModel.self) private var model
    let chain: AppModel.Chain
    let current: ToolID

    var body: some View {
        HStack(spacing: 6) {
            Image(systemName: "link").foregroundStyle(.secondary)
            Image(nsImage: NSWorkspace.shared.icon(forFile: chain.origin.path))
                .resizable().frame(width: 16, height: 16)
            Text(chain.origin.lastPathComponent).lineLimit(1).truncationMode(.middle)
            ForEach(Array(chain.steps.enumerated()), id: \.offset) { _, step in
                Image(systemName: "chevron.right").font(.caption).foregroundStyle(.tertiary)
                if step == current {
                    Text(step.title).fontWeight(.semibold)
                } else {
                    Button(step.title) { model.show(step) }
                        .buttonStyle(.link)
                        .help("Back to this step. Change it, then Then again to redo the steps after it.")
                }
            }
            Spacer(minLength: 12)
            Text("Saves next to \(chain.origin.lastPathComponent)")
                .font(.caption).foregroundStyle(.secondary).lineLimit(1)
            Button {
                model.endChain()
            } label: {
                Image(systemName: "xmark.circle.fill").foregroundStyle(.tertiary)
            }
            .buttonStyle(.plain)
            .help("End the chain: the steps so far are thrown away, and tools save next to their files again")
        }
        .font(.callout)
        .padding(.horizontal, 12)
        .padding(.vertical, 7)
        .background(.bar)
        .overlay(alignment: .bottom) { Divider() }
    }
}
