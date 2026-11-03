import AppKit
import SwiftUI
import UniformTypeIdentifiers

struct ContentView: View {
    @Environment(AppModel.self) private var model
    @State private var dropHover = false

    var body: some View {
        HSplitView {
            fileArea
                .frame(minWidth: 420, maxWidth: .infinity, maxHeight: .infinity)
            OptionsView()
                .frame(minWidth: 300, idealWidth: 330, maxWidth: 400)
        }
        .toolbar { toolbar }
        .onChange(of: optionsKey) { model.optionsChanged() }
    }

    /// Changes whenever any option does, so the preview re-runs.
    private var optionsKey: String {
        [model.format, model.maxSize, model.resize, "\(model.useQuality)\(Int(model.quality))",
         model.speed, "\(model.shiftPitch)", model.start, model.end, "\(model.strip)",
         model.outputFolder?.path ?? ""].joined(separator: "|")
    }

    @ToolbarContentBuilder
    private var toolbar: some ToolbarContent {
        ToolbarItemGroup(placement: .navigation) {
            Button { pickFiles(model) } label: { Label("Add Files", systemImage: "plus") }
            Button { model.clear() } label: { Label("Clear", systemImage: "trash") }
                .disabled(model.files.isEmpty || model.isRunning)
        }
        ToolbarItem(placement: .primaryAction) {
            if model.isRunning {
                Button("Stop") { model.cancel() }
            } else {
                Button {
                    model.convertAll()
                } label: {
                    Text(model.readyCount > 1 ? "Convert \(model.readyCount) Files" : "Convert")
                }
                .buttonStyle(.borderedProminent)
                .disabled(model.readyCount == 0)
                .keyboardShortcut(.return, modifiers: .command)
            }
        }
    }

    @ViewBuilder
    private var fileArea: some View {
        Group {
            if let problem = model.setupProblem {
                ContentUnavailableView {
                    Label("conv not found", systemImage: "exclamationmark.triangle")
                } description: {
                    Text(problem)
                } actions: {
                    SettingsLink { Text("Open Settings") }
                }
            } else if model.files.isEmpty {
                emptyDrop
            } else {
                List {
                    ForEach(model.files) { item in
                        FileRow(item: item)
                    }
                }
                .listStyle(.inset)
            }
        }
        .overlay {
            if dropHover {
                RoundedRectangle(cornerRadius: 10)
                    .strokeBorder(Color.accentColor, style: StrokeStyle(lineWidth: 3, dash: [8]))
                    .padding(6)
                    .allowsHitTesting(false)
            }
        }
        .dropDestination(for: URL.self) { urls, _ in
            model.add(urls)
            return true
        } isTargeted: { dropHover = $0 }
    }

    private var emptyDrop: some View {
        VStack(spacing: 14) {
            Image(systemName: "arrow.down.doc")
                .font(.system(size: 44, weight: .light))
                .foregroundStyle(.secondary)
            Text("Drop files here")
                .font(.title2.weight(.semibold))
            Text("Videos, pictures, audio, documents. Nothing leaves your Mac.")
                .foregroundStyle(.secondary)
            Button("Choose Files…") { pickFiles(model) }
                .controlSize(.large)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .contentShape(Rectangle())
    }
}

// MARK: - File row

struct FileRow: View {
    @Environment(AppModel.self) private var model
    let item: FileItem

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(nsImage: NSWorkspace.shared.icon(forFile: item.url.path))
                .resizable()
                .frame(width: 32, height: 32)
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    Text(item.url.lastPathComponent).lineLimit(1).truncationMode(.middle)
                    if let size = item.size {
                        Text(bytes(size)).foregroundStyle(.secondary).font(.callout)
                    }
                }
                detail
            }
            Spacer(minLength: 8)
            trailing
        }
        .padding(.vertical, 4)
        .contextMenu {
            Button("Show in Finder") { NSWorkspace.shared.activateFileViewerSelecting([item.url]) }
            Button("Remove") { model.remove(item) }.disabled(model.isRunning)
        }
    }

    @ViewBuilder
    private var detail: some View {
        switch item.status {
        case .done(let output, let size):
            Label {
                Text(output.lastPathComponent)
                    + Text(size.map { "  \(bytes($0))\(change($0))" } ?? "").foregroundStyle(.secondary)
            } icon: {
                Image(systemName: "checkmark.circle.fill").foregroundStyle(.green)
            }
            .font(.callout)
            notesList(item.notes, color: .secondary)
        case .failed(let message):
            line(message, systemImage: "xmark.octagon.fill", color: .red)
        case .exists(let message):
            line(message, systemImage: "doc.on.doc.fill", color: .orange)
        case .needsConfirm(let message):
            line(message, systemImage: "exclamationmark.triangle.fill", color: .orange)
        case .idle, .running:
            if let problem = item.problem {
                line(problem, systemImage: "exclamationmark.circle", color: .secondary)
            } else if let out = item.plannedOutput {
                Label(out.lastPathComponent, systemImage: "arrow.turn.down.right")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                notesList(item.warnings, color: .orange)
            } else {
                Text("Checking…").font(.callout).foregroundStyle(.tertiary)
            }
        }
    }

    @ViewBuilder
    private var trailing: some View {
        switch item.status {
        case .running:
            ProgressView().controlSize(.small)
        case .done(let output, _):
            Button("Show") { NSWorkspace.shared.activateFileViewerSelecting([output]) }
                .controlSize(.small)
        case .exists:
            HStack {
                Button("Replace") { model.retry(item, extra: ["-y"]) }
                    .controlSize(.small)
                    .disabled(model.isRunning)
            }
        case .needsConfirm:
            Button("Convert Anyway") { model.retry(item, extra: ["--yes"]) }
                .controlSize(.small)
                .disabled(model.isRunning)
        default:
            Button {
                model.remove(item)
            } label: {
                Image(systemName: "xmark.circle.fill").foregroundStyle(.tertiary)
            }
            .buttonStyle(.plain)
            .disabled(model.isRunning)
            .help("Remove from the list")
        }
    }

    private func line(_ text: String, systemImage: String, color: Color) -> some View {
        Label {
            Text(text).fixedSize(horizontal: false, vertical: true)
        } icon: {
            Image(systemName: systemImage).foregroundStyle(color)
        }
        .font(.callout)
    }

    @ViewBuilder
    private func notesList(_ notes: [String], color: Color) -> some View {
        ForEach(notes, id: \.self) { note in
            Text(note)
                .font(.caption)
                .foregroundStyle(color)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private func change(_ new: Int64) -> String {
        guard let old = item.size, old > 0 else { return "" }
        let pct = Int((Double(new) / Double(old) * 100).rounded())
        return pct < 100 ? ", \(100 - pct)% smaller" : ""
    }
}

func bytes(_ n: Int64) -> String {
    ByteCountFormatter.string(fromByteCount: n, countStyle: .file)
}
