import Observation
import SwiftUI

/// `conv doctor`, `conv install` and `conv update`, as one page. Nothing is
/// downloaded until the user presses Install or Update.
@Observable
final class BackendsModel {
    struct Doctor: Decodable {
        struct Backend: Decodable, Identifiable {
            struct Remedy: Decodable { var managed: String?; var manual: String? }
            var backend: String
            var found: Bool
            var managed_install: Bool?
            var path: String?
            var version: String?
            var source: String?
            var remediation: Remedy?
            var id: String { backend }
        }
        var backends: [Backend]?
    }
    struct Update: Decodable {
        struct Backend: Decodable {
            var backend: String
            var action: String
            var version: String?
            var pinned_version: String?
            var manual_hint: String?
            var error: ConvError?
        }
        struct Me: Decodable { var version: String?; var install_method: String?; var update_hint: String? }
        var ok: Bool?
        var backends: [Backend]?
        var conv: Me?
    }

    var conv: URL?
    private(set) var doctor: [Doctor.Backend] = []
    private(set) var update: [String: Update.Backend] = [:]
    private(set) var me: Update.Me?
    private(set) var busy: String?
    private(set) var message: String?

    @MainActor
    func refresh() async {
        guard let conv else { return }
        if let out = try? await Conv.run(conv, ["doctor", "--json"]), let d = out.decode(Doctor.self) {
            doctor = d.backends ?? []
        }
        if let out = try? await Conv.run(conv, ["update", "--check", "--json"]), let u = out.decode(Update.self) {
            update = Dictionary((u.backends ?? []).map { ($0.backend, $0) }, uniquingKeysWith: { a, _ in a })
            me = u.conv
        }
    }

    var outdated: [String] { update.values.filter { $0.action == "outdated" }.map(\.backend).sorted() }

    @MainActor
    func install(_ backend: String) {
        run("Installing \(backend)…", ["install", backend, "--json"], done: "\(backend) is installed.")
    }

    @MainActor
    func updateAll() {
        run("Updating…", ["update", "--json"], done: "Backends match the versions this conv pins.")
    }

    @MainActor
    private func run(_ label: String, _ args: [String], done: String) {
        guard let conv else { return }
        busy = label
        message = nil
        Task { @MainActor in
            defer { busy = nil }
            guard let out = try? await Conv.run(conv, args) else { return }
            struct Envelope: Decodable { var ok: Bool; var error: ConvError? }
            let env = out.decode(Envelope.self)
            message = env?.ok == true ? done : (env?.error?.message ?? Conv.stderrMessage(out.stderr))
            await refresh()
        }
    }
}

struct BackendsView: View {
    @Bindable var backends: BackendsModel

    private static let purpose = [
        "ffmpeg": "Video, audio and GIF",
        "ffprobe": "Reads video and audio details",
        "magick": "Pictures (ImageMagick)",
        "pandoc": "Documents: Markdown, Word, HTML",
        "typst": "PDFs from documents",
        "soffice": "Office files (LibreOffice)",
    ]

    var body: some View {
        Form {
            Section {
                ForEach(backends.doctor) { b in
                    row(b)
                }
            } header: {
                Text("Backends")
            } footer: {
                Text("conv uses these programs for the actual work. Install downloads a copy whose checksum this conv has pinned.")
                    .font(.caption).foregroundStyle(.secondary)
            }

            Section("Updates") {
                if backends.outdated.isEmpty {
                    Label("Everything conv manages is on the version it pins.", systemImage: "checkmark.circle")
                        .foregroundStyle(.secondary)
                } else {
                    LabeledContent("Out of date: \(backends.outdated.joined(separator: ", "))") {
                        Button("Update") { backends.updateAll() }.disabled(backends.busy != nil)
                    }
                }
                if let me = backends.me {
                    LabeledContent("conv \(me.version ?? "")") {
                        if let hint = me.update_hint {
                            Text(hint).font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                        }
                    }
                }
            }

            if backends.busy != nil || backends.message != nil {
                Section {
                    if let busy = backends.busy {
                        HStack { ProgressView().controlSize(.small); Text(busy) }
                    } else if let message = backends.message {
                        Text(message)
                    }
                }
            }
        }
        .formStyle(.grouped)
        .navigationTitle("Backends")
        .toolbar {
            Button { Task { await backends.refresh() } } label: { Label("Check Again", systemImage: "arrow.clockwise") }
        }
        .task { await backends.refresh() }
    }

    @ViewBuilder
    private func row(_ b: BackendsModel.Doctor.Backend) -> some View {
        let u = backends.update[b.backend]
        HStack(alignment: .top) {
            Image(systemName: b.found ? "checkmark.circle.fill" : "circle.dashed")
                .foregroundStyle(b.found ? .green : .secondary)
            VStack(alignment: .leading, spacing: 2) {
                HStack {
                    Text(b.backend).font(.body.weight(.medium))
                    if let v = b.version { Text(v).foregroundStyle(.secondary) }
                    if u?.action == "outdated", let pin = u?.pinned_version {
                        Text("(\(pin) available)").foregroundStyle(.orange)
                    }
                }
                Text(Self.purpose[b.backend] ?? "").font(.caption).foregroundStyle(.secondary)
                if let path = b.path {
                    Text(path).font(.caption2).foregroundStyle(.tertiary).textSelection(.enabled)
                } else if let manual = b.remediation?.manual ?? u?.manual_hint {
                    Text(manual).font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                }
            }
            Spacer()
            if !b.found {
                if b.managed_install == true {
                    Button("Install") { backends.install(b.backend) }.disabled(backends.busy != nil)
                } else {
                    Text("Install it yourself").font(.caption).foregroundStyle(.secondary)
                }
            }
        }
    }
}
