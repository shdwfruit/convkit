import AppKit
import Observation
import SwiftUI

/// `conv qr`: the preview is a real code written to a temp folder, so what you see
/// is what conv made and read back.
@Observable
final class QRModel {
    var conv: URL?
    var text = ""
    var logo: URL?

    private(set) var preview: NSImage?
    private(set) var info = ""
    private(set) var problem: String?
    private(set) var saved: URL?
    private(set) var working = false

    private var task: Task<Void, Never>?
    private let tmp = FileManager.default.temporaryDirectory.appendingPathComponent("convkit-qr", isDirectory: true)

    func changed() {
        task?.cancel()
        saved = nil
        guard let conv, !text.isEmpty else {
            preview = nil
            info = ""
            problem = nil
            return
        }
        let text = text
        let logo = logo
        task = Task { @MainActor in
            try? await Task.sleep(for: .milliseconds(250))
            guard !Task.isCancelled else { return }
            try? FileManager.default.createDirectory(at: tmp, withIntermediateDirectories: true)
            let out = tmp.appendingPathComponent("preview-\(UUID().uuidString).png")
            defer { try? FileManager.default.removeItem(at: out) }
            var args = ["qr", "--json", "--no-install", "-y", "--size", "600"]
            if let logo { args += ["--logo", logo.path] }
            args += ["--", text, out.path]
            working = true
            defer { working = false }
            guard let result = try? await Conv.run(conv, args), !Task.isCancelled else { return }
            let (r, err) = Self.parse(result)
            if let r, r.ok {
                preview = (try? Data(contentsOf: out)).flatMap(NSImage.init(data:))
                problem = nil
                info = Self.describe(r.qr)
            } else {
                preview = nil
                problem = err ?? "conv couldn't make that code."
            }
        }
    }

    @MainActor
    func save() {
        guard let conv, !text.isEmpty,
              let url = askWhereToSave(name: "qr.png", types: ["png", "svg", "jpg", "webp", "pdf"])
        else { return }
        // The save panel has already asked about replacing, so -y is the user's answer.
        var args = ["qr", "--json", "--no-install", "-y"]
        if let logo { args += ["--logo", logo.path] }
        args += ["--", text, url.path]
        task = Task { @MainActor in
            working = true
            defer { working = false }
            guard let result = try? await Conv.run(conv, args) else { return }
            let (r, err) = Self.parse(result)
            if let r, r.ok {
                saved = r.output.map(URL.init(fileURLWithPath:)) ?? url
                problem = nil
            } else {
                problem = err ?? "conv couldn't save the code."
            }
        }
    }

    @MainActor
    func copyImage() {
        guard let preview else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.writeObjects([preview])
    }

    struct Result: Decodable {
        struct Info: Decodable {
            var version: Int?
            var error_correction: String?
            var size: Int?
        }
        var ok: Bool
        var output: String?
        var qr: Info?
        var error: ConvError?
    }

    private struct Response: Decodable {
        var ok: Bool
        var results: [Result]?
        var error: ConvError?
    }

    private static func parse(_ out: Conv.Output) -> (Result?, String?) {
        guard let resp = out.decode(Response.self) else { return (nil, Conv.stderrMessage(out.stderr)) }
        let r = resp.results?.first
        return (r, (r?.error ?? resp.error).map(AppModel.friendly))
    }

    private static func describe(_ info: Result.Info?) -> String {
        guard let info else { return "" }
        var parts = ["Checked: it scans back to exactly this text."]
        if let level = info.error_correction {
            let pct = ["L": "7%", "M": "15%", "Q": "25%", "H": "30%"][level] ?? level
            parts.append("Still reads with \(pct) of it covered or damaged.")
        }
        return parts.joined(separator: " ")
    }
}

struct QRView: View {
    @Bindable var qr: QRModel

    var body: some View {
        HSplitView {
            VStack(alignment: .leading, spacing: 14) {
                Text("What the code holds").font(.headline)
                TextEditor(text: $qr.text)
                    .font(.body)
                    .frame(minHeight: 90, maxHeight: 160)
                    .overlay(alignment: .topLeading) {
                        if qr.text.isEmpty {
                            Text("https://example.com, or any text")
                                .foregroundStyle(.tertiary)
                                .padding(.leading, 5)
                                .allowsHitTesting(false)
                        }
                    }
                    .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(.quaternary))
                Text("The text is in the code itself. No redirect service, so it never expires and nobody counts scans.")
                    .font(.caption)
                    .foregroundStyle(.secondary)

                Divider()

                HStack {
                    Text("Logo").font(.headline)
                    Spacer()
                    if let logo = qr.logo {
                        Text(logo.lastPathComponent).lineLimit(1).truncationMode(.middle)
                            .foregroundStyle(.secondary)
                        Button("Remove") { qr.logo = nil }
                    } else {
                        Button("Choose…") { chooseLogo() }
                    }
                }
                Text("Sits in the middle. The code gets stronger error correction so it still scans.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Spacer()
            }
            .padding(20)
            .frame(minWidth: 280, idealWidth: 320, maxWidth: 420)

            VStack(spacing: 16) {
                Spacer()
                ZStack {
                    RoundedRectangle(cornerRadius: 12).fill(.background.secondary)
                    if let image = qr.preview {
                        Image(nsImage: image)
                            .resizable()
                            .interpolation(.none)
                            .aspectRatio(contentMode: .fit)
                            .padding(18)
                            .draggable(image)
                    } else if let problem = qr.problem {
                        Label(problem, systemImage: "exclamationmark.triangle")
                            .foregroundStyle(.orange)
                            .padding()
                    } else {
                        Image(systemName: "qrcode")
                            .font(.system(size: 80, weight: .ultraLight))
                            .foregroundStyle(.quaternary)
                    }
                    if qr.working {
                        ProgressView().controlSize(.small)
                            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topTrailing)
                            .padding(10)
                    }
                }
                .frame(width: 320, height: 320)

                Text(qr.info).font(.caption).foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .frame(maxWidth: 340)

                HStack {
                    Button("Copy Image") { qr.copyImage() }
                        .disabled(qr.preview == nil)
                    Button("Save…") { qr.save() }
                        .buttonStyle(.borderedProminent)
                        .disabled(qr.preview == nil)
                }
                if let saved = qr.saved {
                    Button("Saved \(saved.lastPathComponent). Show in Finder") {
                        NSWorkspace.shared.activateFileViewerSelecting([saved])
                    }
                    .buttonStyle(.link)
                }
                Spacer()
            }
            .frame(minWidth: 380, maxWidth: .infinity)
            .padding()
        }
        .navigationTitle("QR Code")
        .onChange(of: qr.text) { qr.changed() }
        .onChange(of: qr.logo) { qr.changed() }
    }

    private func chooseLogo() {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = [.svg, .png, .jpeg, .gif, .webP]
        if panel.runModal() == .OK { qr.logo = panel.url }
    }
}
