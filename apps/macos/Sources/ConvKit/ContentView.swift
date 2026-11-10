import AppKit
import SwiftUI
import UniformTypeIdentifiers

struct ConvertView: View {
    @Environment(AppModel.self) private var model
    @Bindable var job: ConvertJob
    @State private var dropHover = false

    var body: some View {
        HSplitView {
            fileArea
                .frame(minWidth: 400, maxWidth: .infinity, maxHeight: .infinity)
            OptionsView(job: job, convVersion: model.convVersion,
                        moreOptions: job.isGeneral ? nil : { model.moreOptions(from: job) })
                .frame(minWidth: 300, idealWidth: 330, maxWidth: 400)
        }
        .navigationTitle(job.tool.title)
        .toolbar { toolbar }
        .onChange(of: job.settingsKey) { job.optionsChanged() }
    }

    @ToolbarContentBuilder
    private var toolbar: some ToolbarContent {
        ToolbarItemGroup(placement: .primaryAction) {
            Button { pickFiles(job) } label: { Label("Add Files", systemImage: "plus") }
                .help("Add files (⌘O)")
            Button { Task { await job.paste() } } label: {
                Label("Paste", systemImage: "doc.on.clipboard")
            }
            .help("Add files copied in Finder, or a copied picture or screenshot (⇧⌘V)")
            .keyboardShortcut("v", modifiers: [.command, .shift])
            .disabled(job.isRunning)
            Button { job.clear() } label: { Label("Clear", systemImage: "trash") }
                .help("Clear the list")
                .disabled(job.files.isEmpty || job.isRunning)
            ThenMenu(tools: model.nextTools(after: job.tool, making: job.fitting.compactMap(\.plannedOutput)),
                     disabled: job.readyCount == 0 || job.isRunning) { next in
                Task { await model.chain(job, to: next) }
            }
            if job.isRunning {
                Button("Stop") { job.cancel() }
            } else {
                Button {
                    job.runAll()
                } label: {
                    Text(job.readyCount > 1 ? "\(job.preset.action) \(job.readyCount) Files" : job.preset.action)
                }
                .buttonStyle(.borderedProminent)
                .disabled(job.readyCount == 0)
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
            } else if job.files.isEmpty {
                emptyDrop
            } else {
                List {
                    ForEach(job.files) { item in
                        FileRow(job: job, item: item)
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
            job.add(urls)
            return true
        } isTargeted: { dropHover = $0 }
        .onPasteCommand(of: [.fileURL, .image]) { _ in Task { await job.paste() } }
        .safeAreaInset(edge: .bottom) {
            if let note = job.pasteProblem ?? job.clipboardNote {
                HStack {
                    Image(systemName: job.pasteProblem == nil ? "doc.on.clipboard" : "exclamationmark.circle")
                    Text(note).font(.callout)
                    Spacer()
                }
                .padding(8)
                .background(.bar)
            }
        }
    }

    private var emptyDrop: some View {
        VStack(spacing: 14) {
            ToolIcon(tool: job.tool, size: 52)
            Text(job.preset.dropTitle)
                .font(.title2.weight(.semibold))
            Text(job.preset.dropDetail)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 380)
            Button("Choose Files…") { pickFiles(job) }
                .controlSize(.large)
        }
        .padding()
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .contentShape(Rectangle())
    }
}

// MARK: - File row

struct FileRow: View {
    let job: ConvertJob
    let item: FileItem

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(nsImage: NSWorkspace.shared.icon(forFile: item.url.path))
                .resizable()
                .frame(width: 32, height: 32)
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    Text(item.displayName).lineLimit(1).truncationMode(.middle)
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
        .opacity(item.misfit == nil ? 1 : 0.6)
        .contextMenu {
            Button("Show in Finder") { NSWorkspace.shared.activateFileViewerSelecting([item.url]) }
            Button("Remove") { job.remove(item) }.disabled(job.isRunning)
        }
    }

    @ViewBuilder
    private var detail: some View {
        if let misfit = item.misfit {
            line(misfit, systemImage: "nosign", color: .secondary)
        } else {
            status
        }
    }

    @ViewBuilder
    private var status: some View {
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
                // Stills and PDF pages go into a folder, named without an extension.
                let name = out.pathExtension.isEmpty ? out.lastPathComponent + "/" : out.lastPathComponent
                Label(item.pasted && job.outputFolder == nil
                      ? "\(name) in \(out.deletingLastPathComponent().lastPathComponent)"
                      : name,
                      systemImage: "arrow.turn.down.right")
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
            HStack {
                OpenInMenu(output: output, from: job.tool)
                Button("Show") { NSWorkspace.shared.activateFileViewerSelecting([output]) }
                    .controlSize(.small)
            }
        case .exists:
            HStack {
                Button("Replace") { job.retry(item, extra: ["-y"]) }
                    .controlSize(.small)
                    .disabled(job.isRunning)
            }
        case .needsConfirm:
            Button("Convert Anyway") { job.retry(item, extra: ["--yes"]) }
                .controlSize(.small)
                .disabled(job.isRunning)
        default:
            Button {
                job.remove(item)
            } label: {
                Image(systemName: "xmark.circle.fill").foregroundStyle(.tertiary)
            }
            .buttonStyle(.plain)
            .disabled(job.isRunning)
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
