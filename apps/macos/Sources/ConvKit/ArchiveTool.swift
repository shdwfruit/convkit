import AppKit
import Observation
import SwiftUI

/// What `conv` says about an archive it makes or opens.
private struct ArchiveResponse: Decodable {
    struct Listing: Decodable {
        struct Entry: Decodable { var kind: String?; var name: String; var size: Int64? }
        var encrypted: Bool?
        var entries: [Entry]?
    }
    struct Plan: Decodable {
        var output: String?
        var listing: Listing?
        var warnings: [String]?
        var notes: [String]?
    }
    struct Archive: Decodable {
        var files: Int?
        var folders: Int?
        var content_bytes: Int64?
        var encrypted: Bool?
    }
    struct Entry: Decodable {
        var ok: Bool
        var plan: Plan?
        var output: String?
        var bytes: Int64?
        var archive: Archive?
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

// MARK: - Make

/// Packs files and folders into one archive: `conv a b folder/ .zip -o DIR`.
@Observable
final class MakeArchiveModel {
    var conv: URL?
    private(set) var items: [URL] = []
    var format = "zip"
    let formats = ["zip", "tar.gz", "tar.xz", "tar.bz2", "tar.zst", "tar"]
    var lock = false
    var password = ""
    var again = ""
    /// Where the archive goes; nil is next to the first item.
    var outputFolder: URL?

    private(set) var planned: URL?
    private(set) var notes: [String] = []
    private(set) var problem: String?
    private(set) var result: String?
    private(set) var written: URL?
    private(set) var exists = false
    private(set) var working = false
    private var task: Task<Void, Never>?

    var passwordReason: String? { format == "zip" ? nil : "Only a zip takes a password." }
    private var locking: Bool { lock && passwordReason == nil && !password.isEmpty }

    @MainActor
    func add(_ urls: [URL]) {
        let known = Set(items.map(\.standardizedFileURL))
        items += urls.filter { !known.contains($0.standardizedFileURL) }
        changed()
    }

    @MainActor
    func remove(_ url: URL) {
        items.removeAll { $0 == url }
        changed()
    }

    private var args: [String]? {
        guard let first = items.first else { return nil }
        // Several files would otherwise land in conv's own folder, so name the folder.
        return items.map(\.path) + ["." + format, "-o", (outputFolder ?? first.deletingLastPathComponent()).path]
            + (locking ? ["--password"] : [])
    }

    private var env: [String: String] { locking ? ["CONVKIT_PASSWORD": password] : [:] }

    func changed() {
        task?.cancel()
        exists = false
        written = nil
        result = nil
        guard let conv, let args else {
            planned = nil
            problem = nil
            notes = []
            return
        }
        if lock && passwordReason == nil && !password.isEmpty && password != again {
            problem = again.isEmpty ? "Type the password again." : "The two passwords don't match."
            return
        }
        task = Task { @MainActor in
            try? await Task.sleep(for: .milliseconds(200))
            guard !Task.isCancelled,
                  let out = try? await Conv.run(conv, args + ["--dry-run", "--json", "--no-install"], env: env),
                  !Task.isCancelled else { return }
            guard let resp = out.decode(ArchiveResponse.self) else {
                problem = Conv.stderrMessage(out.stderr)
                return
            }
            problem = resp.problem
            planned = resp.entry?.plan?.output.map(URL.init(fileURLWithPath:))
            notes = (resp.entry?.plan?.warnings ?? []) + (resp.entry?.plan?.notes ?? [])
        }
    }

    var ready: Bool { planned != nil && problem == nil && !working && !(lock && passwordReason == nil && password.isEmpty) }

    @MainActor
    func make(replace: Bool = false) {
        guard let conv, let args else { return }
        task?.cancel()
        task = Task { @MainActor in
            working = true
            defer { working = false }
            guard let out = try? await Conv.run(conv, args + ["--json", "--no-install"] + (replace ? ["-y"] : []), env: env),
                  let resp = out.decode(ArchiveResponse.self) else {
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
            notes = (resp.entry?.warnings ?? []) + (resp.entry?.notes ?? [])
            if let a = resp.entry?.archive {
                var parts = ["\(a.files ?? 0) files"]
                if let b = resp.entry?.bytes, let c = a.content_bytes { parts.append("\(bytes(c)) in, \(bytes(b)) out") }
                if a.encrypted == true { parts.append("locked with AES-256") }
                result = parts.joined(separator: ", ")
            }
        }
    }
}

struct MakeArchiveView: View {
    @Bindable var archive: MakeArchiveModel
    let paste: () async -> [URL]

    var body: some View {
        HSplitView {
            Group {
                if archive.items.isEmpty {
                    VStack(spacing: 14) {
                        ToolIcon(tool: .makeArchive, size: 52)
                        Text("Drop files and folders to pack").font(.title2.weight(.semibold))
                        Text("Times, the executable bit and links are kept. Finder's .DS_Store and __MACOSX files are left out.")
                            .foregroundStyle(.secondary).multilineTextAlignment(.center).frame(maxWidth: 400)
                        Button("Choose…") { choose() }.controlSize(.large)
                    }
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                } else {
                    List {
                        ForEach(archive.items, id: \.self) { url in
                            HStack {
                                Image(nsImage: NSWorkspace.shared.icon(forFile: url.path)).resizable().frame(width: 24, height: 24)
                                Text(url.lastPathComponent).lineLimit(1).truncationMode(.middle)
                                Spacer()
                                Button { archive.remove(url) } label: {
                                    Image(systemName: "xmark.circle.fill").foregroundStyle(.tertiary)
                                }
                                .buttonStyle(.plain)
                            }
                        }
                    }
                }
            }
            .frame(minWidth: 360, maxWidth: .infinity, maxHeight: .infinity)
            .dropDestination(for: URL.self) { urls, _ in archive.add(urls); return true }

            Form {
                Section {
                    Picker("Kind", selection: $archive.format) {
                        ForEach(archive.formats, id: \.self) { Text($0 == "zip" ? "Zip" : $0).tag($0) }
                    }
                } footer: {
                    Text("Zip opens anywhere. tar.gz and the other tars keep Unix permissions, and tar.xz and tar.zst pack tighter.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                Section {
                    Toggle("Lock with a password", isOn: $archive.lock).why(archive.passwordReason)
                    if archive.lock && archive.passwordReason == nil {
                        SecureField("Password", text: $archive.password)
                        SecureField("Again", text: $archive.again)
                    }
                } header: {
                    Text("Password")
                } footer: {
                    Text(archive.passwordReason ?? "AES-256. The password goes to conv privately, never on a command line.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                Section(archive.written == nil ? "Will write" : "Wrote") {
                    if let p = archive.problem {
                        Label(p, systemImage: "exclamationmark.triangle").foregroundStyle(.orange)
                        if archive.exists { Button("Replace It") { archive.make(replace: true) } }
                    }
                    if let w = archive.written {
                        Button(w.lastPathComponent) { NSWorkspace.shared.activateFileViewerSelecting([w]) }
                            .buttonStyle(.link)
                        if let r = archive.result { Text(r).font(.caption).foregroundStyle(.secondary) }
                    } else if let p = archive.planned {
                        LabeledContent(p.lastPathComponent, value: p.deletingLastPathComponent().lastPathComponent)
                    } else if archive.items.isEmpty {
                        Text("Add files first.").foregroundStyle(.secondary)
                    }
                    ForEach(archive.notes, id: \.self) { Text($0).font(.caption).foregroundStyle(.secondary) }
                }
            }
            .formStyle(.grouped)
            .frame(minWidth: 300, idealWidth: 330, maxWidth: 400)
        }
        .navigationTitle("Zip Files")
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                Button { choose() } label: { Label("Add", systemImage: "plus") }
                Button { Task { archive.add(await paste()) } } label: { Label("Paste", systemImage: "doc.on.clipboard") }
                    .help("Add files copied in Finder")
                Button("Make \(archive.format == "zip" ? "Zip" : archive.format)") { archive.make() }
                    .buttonStyle(.borderedProminent)
                    .disabled(!archive.ready)
            }
        }
        .onChange(of: archive.format) { archive.changed() }
        .onChange(of: archive.lock) { archive.changed() }
        .onChange(of: archive.password) { archive.changed() }
        .onChange(of: archive.again) { archive.changed() }
    }

    private func choose() {
        let panel = NSOpenPanel()
        panel.allowsMultipleSelection = true
        panel.canChooseDirectories = true
        if panel.runModal() == .OK { archive.add(panel.urls) }
    }
}

// MARK: - Open

/// Unpacks an archive beside itself or into a folder: `conv photos.zip [-o DIR]`.
@Observable
final class OpenArchiveModel {
    var conv: URL?
    private(set) var file: URL?
    var outputFolder: URL?
    var password = ""

    private(set) var entries: [String] = []
    private(set) var totalBytes: Int64 = 0
    private(set) var encrypted = false
    private(set) var planned: URL?
    private(set) var problem: String?
    private(set) var notes: [String] = []
    private(set) var written: URL?
    private(set) var exists = false
    private(set) var working = false
    private var task: Task<Void, Never>?

    static let exts: Set<String> = ["zip", "tar", "tar.gz", "tgz", "tar.xz", "tar.bz2", "tar.zst", "7z"]

    @MainActor
    func load(_ url: URL) {
        file = url
        written = nil
        changed()
    }

    private var args: [String]? {
        guard let file else { return nil }
        return [file.path] + (outputFolder.map { ["-o", $0.path] } ?? [])
    }

    private var env: [String: String] { password.isEmpty ? [:] : ["CONVKIT_PASSWORD": password] }

    func changed() {
        task?.cancel()
        exists = false
        guard let conv, let args else { return }
        task = Task { @MainActor in
            try? await Task.sleep(for: .milliseconds(150))
            guard !Task.isCancelled,
                  let out = try? await Conv.run(conv, args + ["--dry-run", "--json", "--no-install"], env: env),
                  !Task.isCancelled, let resp = out.decode(ArchiveResponse.self) else { return }
            problem = resp.problem
            let plan = resp.entry?.plan
            planned = plan?.output.map(URL.init(fileURLWithPath:))
            encrypted = plan?.listing?.encrypted ?? false
            let files = (plan?.listing?.entries ?? []).filter { $0.kind != "folder" }
            entries = files.map(\.name)
            totalBytes = files.compactMap(\.size).reduce(0, +)
            notes = (plan?.warnings ?? []) + (plan?.notes ?? [])
        }
    }

    var ready: Bool { planned != nil && problem == nil && !working && !(encrypted && password.isEmpty) }

    @MainActor
    func open(replace: Bool = false) {
        guard let conv, let args else { return }
        task?.cancel()
        task = Task { @MainActor in
            working = true
            defer { working = false }
            guard let out = try? await Conv.run(conv, args + ["--json", "--no-install"] + (replace ? ["-y"] : []), env: env),
                  let resp = out.decode(ArchiveResponse.self) else {
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
            notes = (resp.entry?.warnings ?? []) + (resp.entry?.notes ?? [])
        }
    }
}

struct OpenArchiveView: View {
    @Bindable var archive: OpenArchiveModel
    let paste: () async -> [URL]

    var body: some View {
        HSplitView {
            Group {
                if let file = archive.file {
                    VStack(alignment: .leading, spacing: 0) {
                        HStack(spacing: 10) {
                            Image(nsImage: NSWorkspace.shared.icon(forFile: file.path)).resizable().frame(width: 32, height: 32)
                            VStack(alignment: .leading) {
                                Text(file.lastPathComponent).font(.headline)
                                Text("\(archive.entries.count) files, \(bytes(archive.totalBytes))\(archive.encrypted ? ", locked" : "")")
                                    .font(.caption).foregroundStyle(.secondary)
                            }
                        }
                        .padding(12)
                        Divider()
                        List(archive.entries.prefix(500), id: \.self) { name in
                            Text(name).font(.system(.callout, design: .monospaced)).lineLimit(1).truncationMode(.middle)
                        }
                    }
                } else {
                    VStack(spacing: 14) {
                        ToolIcon(tool: .openArchive, size: 52)
                        Text("Drop an archive to open").font(.title2.weight(.semibold))
                        Text("Zip, tar, tar.gz, tar.xz, tar.bz2, tar.zst and 7z. One with a file that would land outside its folder is refused whole.")
                            .foregroundStyle(.secondary).multilineTextAlignment(.center).frame(maxWidth: 400)
                        Button("Choose…") { choose() }.controlSize(.large)
                    }
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                }
            }
            .frame(minWidth: 360, maxWidth: .infinity, maxHeight: .infinity)
            .dropDestination(for: URL.self) { urls, _ in
                guard let first = urls.first, OpenArchiveModel.exts.contains(fileExt(first)) else { return false }
                archive.load(first)
                return true
            }

            Form {
                if archive.encrypted {
                    Section {
                        SecureField("Password", text: $archive.password)
                    } header: {
                        Text("Password")
                    } footer: {
                        Text("It's locked. The password goes to conv privately.").font(.caption).foregroundStyle(.secondary)
                    }
                }
                Section("Open into") {
                    Picker("Open into", selection: Binding(
                        get: { archive.outputFolder == nil ? 0 : 1 },
                        set: { if $0 == 0 { archive.outputFolder = nil } else { chooseFolder() } }
                    )) {
                        Text("Next to the archive").tag(0)
                        Text(archive.outputFolder?.lastPathComponent ?? "Choose…").tag(1)
                    }
                    .labelsHidden()
                    .pickerStyle(.radioGroup)
                }
                Section(archive.written == nil ? "Will write" : "Wrote") {
                    if let p = archive.problem {
                        Label(p, systemImage: "exclamationmark.triangle").foregroundStyle(.orange)
                        if archive.exists { Button("Replace It") { archive.open(replace: true) } }
                    }
                    if let w = archive.written {
                        Button(w.lastPathComponent) { NSWorkspace.shared.activateFileViewerSelecting([w]) }.buttonStyle(.link)
                    } else if let p = archive.planned {
                        LabeledContent(p.lastPathComponent, value: p.deletingLastPathComponent().lastPathComponent)
                    }
                    ForEach(archive.notes, id: \.self) { Text($0).font(.caption).foregroundStyle(.secondary) }
                }
            }
            .formStyle(.grouped)
            .frame(minWidth: 300, idealWidth: 330, maxWidth: 400)
        }
        .navigationTitle("Open Archive")
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                Button { choose() } label: { Label("Open", systemImage: "folder") }
                Button {
                    Task { if let f = await paste().first(where: { OpenArchiveModel.exts.contains(fileExt($0)) }) { archive.load(f) } }
                } label: { Label("Paste", systemImage: "doc.on.clipboard") }
                    .help("Open an archive copied in Finder")
                Button("Unpack") { archive.open() }
                    .buttonStyle(.borderedProminent)
                    .disabled(!archive.ready)
            }
        }
        .onChange(of: archive.outputFolder) { archive.changed() }
        .onChange(of: archive.password) { archive.changed() }
    }

    private func choose() {
        let panel = NSOpenPanel()
        if panel.runModal() == .OK, let url = panel.url, OpenArchiveModel.exts.contains(fileExt(url)) { archive.load(url) }
    }

    private func chooseFolder() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.prompt = "Open Here"
        if panel.runModal() == .OK, let url = panel.url { archive.outputFolder = url }
    }
}
