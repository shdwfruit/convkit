import Observation
import SwiftUI

/// `conv scan`: what each file in a folder could become, by extension alone.
@Observable
final class ScanModel {
    struct Entry: Decodable, Identifiable, Hashable {
        var path: String
        var format: String?
        var kind: String?
        var convertible: Bool
        var targets: [String]?
        var id: String { path }
        var name: String { (path as NSString).lastPathComponent }
    }
    private struct Response: Decodable {
        var ok: Bool
        var files: [Entry]?
        var error: ConvError?
    }

    var conv: URL?
    private(set) var folder: URL?
    private(set) var entries: [Entry] = []
    private(set) var problem: String?
    var selection: Set<String> = []

    @MainActor
    func scan(_ url: URL) async {
        folder = url
        selection = []
        guard let conv, let out = try? await Conv.run(conv, ["scan", "--json", url.path]) else { return }
        guard let resp = out.decode(Response.self) else {
            problem = Conv.stderrMessage(out.stderr)
            return
        }
        entries = resp.files ?? []
        problem = resp.error.map(AppModel.friendly)
    }
}

struct ScanView: View {
    @Bindable var scan: ScanModel
    /// Sends files to Convert.
    let convert: ([URL]) -> Void

    var body: some View {
        VStack(spacing: 0) {
            if scan.folder == nil {
                VStack(spacing: 14) {
                    Image(systemName: "folder.badge.questionmark").font(.system(size: 44, weight: .light))
                        .foregroundStyle(.secondary)
                    Text("What could these files become?").font(.title2.weight(.semibold))
                    Text("Pick a folder. Nothing is opened or changed; conv goes by each file's type.")
                        .foregroundStyle(.secondary)
                    Button("Choose Folder…") { choose() }.controlSize(.large)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                if let problem = scan.problem {
                    Label(problem, systemImage: "exclamationmark.triangle").foregroundStyle(.orange).padding(8)
                }
                Table(scan.entries, selection: $scan.selection) {
                    TableColumn("Name") { e in
                        HStack {
                            Image(nsImage: NSWorkspace.shared.icon(forFile: e.path))
                                .resizable().frame(width: 16, height: 16)
                            Text(e.name).lineLimit(1).truncationMode(.middle)
                        }
                    }
                    .width(min: 160, ideal: 260)
                    TableColumn("Kind") { e in Text(e.kind ?? "").foregroundStyle(.secondary) }
                        .width(70)
                    TableColumn("Can become") { e in
                        if let t = e.targets, e.convertible, !t.isEmpty {
                            Text(t.joined(separator: "  ").uppercased()).font(.callout)
                        } else {
                            Text("Nothing conv knows").foregroundStyle(.tertiary)
                        }
                    }
                }
            }
        }
        .navigationTitle("Scan Folder")
        .navigationSubtitle(scan.folder?.path ?? "")
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                Button { choose() } label: { Label("Choose Folder", systemImage: "folder") }
                Button(scan.selection.isEmpty ? "Convert…" : "Convert \(scan.selection.count)…") {
                    let picked = scan.selection.isEmpty
                        ? scan.entries.filter(\.convertible).map(\.path)
                        : Array(scan.selection)
                    convert(picked.sorted().map(URL.init(fileURLWithPath:)))
                }
                .buttonStyle(.borderedProminent)
                .disabled(scan.entries.allSatisfy { !$0.convertible })
                .help("Open these in Convert. With nothing selected, every file conv can convert.")
            }
        }
        .dropDestination(for: URL.self) { urls, _ in
            guard let first = urls.first else { return false }
            Task { await scan.scan(first) }
            return true
        }
    }

    private func choose() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        if panel.runModal() == .OK, let url = panel.url { Task { await scan.scan(url) } }
    }
}
