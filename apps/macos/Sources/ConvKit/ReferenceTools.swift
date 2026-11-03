import Observation
import SwiftUI

// MARK: - Formats (`conv capabilities`)

@Observable
final class FormatsModel {
    struct Pair: Decodable { var from: String; var to: String }
    private struct All: Decodable { var pairs: [Pair]? }

    var conv: URL?
    private(set) var formats: [String] = []
    var selected: String?
    private(set) var detail: CapabilitiesDetail?

    @MainActor
    func load() async {
        guard let conv, formats.isEmpty,
              let out = try? await Conv.run(conv, ["capabilities", "--json"]),
              let all = out.decode(All.self) else { return }
        let names = Set((all.pairs ?? []).flatMap { [$0.from, $0.to] })
        formats = names.sorted()
        if selected == nil { selected = formats.first }
        await show()
    }

    @MainActor
    func show() async {
        guard let conv, let f = selected,
              let out = try? await Conv.run(conv, ["capabilities", f, "--json"]) else { return }
        detail = out.decode(CapabilitiesDetail.self)
    }
}

struct CapabilitiesDetail: Decodable {
    struct Target: Decodable, Identifiable {
        var to: String
        var backends: [String]?
        var defaults: [String: String]?
        var notes: [String]?
        var tuning: [String]?
        var id: String { to }
    }
    var format: String?
    var kind: String?
    var sources: [String]?
    var targets: [Target]?
    var defaults: [String: String]?
}

struct FormatsView: View {
    @Bindable var formats: FormatsModel

    var body: some View {
        HSplitView {
            List(formats.formats, id: \.self, selection: $formats.selected) { f in
                Text(f.uppercased()).tag(f)
            }
            .frame(minWidth: 120, idealWidth: 140, maxWidth: 180)

            ScrollView {
                if let d = formats.detail {
                    VStack(alignment: .leading, spacing: 16) {
                        HStack(alignment: .firstTextBaseline) {
                            Text((d.format ?? "").uppercased()).font(.largeTitle.weight(.semibold))
                            Text(d.kind ?? "").foregroundStyle(.secondary)
                        }
                        if let src = d.sources, !src.isEmpty {
                            labeled("Made from", src.map { $0.uppercased() }.joined(separator: "  "))
                        }
                        if let defs = d.defaults, !defs.isEmpty {
                            labeled("Defaults when it's the target", defs.sorted { $0.key < $1.key }
                                .map { "\($0.key) \($0.value)" }.joined(separator: ", "))
                        }
                        Text("Turns into").font(.headline)
                        ForEach(d.targets ?? []) { t in
                            VStack(alignment: .leading, spacing: 4) {
                                HStack {
                                    Text(t.to.uppercased()).font(.body.weight(.semibold))
                                    Text((t.backends ?? []).joined(separator: " + ")).font(.caption)
                                        .foregroundStyle(.secondary)
                                }
                                if let tuning = t.tuning, !tuning.isEmpty {
                                    Text(tuning.joined(separator: "  "))
                                        .font(.system(.caption, design: .monospaced))
                                        .foregroundStyle(.secondary)
                                }
                                ForEach(t.notes ?? [], id: \.self) {
                                    Text($0).font(.caption).foregroundStyle(.orange)
                                }
                            }
                            .padding(10)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .background(.background.secondary, in: RoundedRectangle(cornerRadius: 8))
                        }
                    }
                    .padding(20)
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
            }
        }
        .navigationTitle("Formats")
        .task { await formats.load() }
        .onChange(of: formats.selected) { Task { await formats.show() } }
    }

    private func labeled(_ title: String, _ value: String) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(title).font(.caption).foregroundStyle(.secondary)
            Text(value).textSelection(.enabled)
        }
    }
}

// MARK: - What's New (`conv changelog`)

@Observable
final class ChangelogModel {
    struct Release: Decodable, Identifiable {
        var version: String
        var date: String?
        var notes: String
        var id: String { version }
    }
    private struct Response: Decodable { var installed: String?; var results: [Release]? }

    var conv: URL?
    private(set) var releases: [Release] = []
    private(set) var installed = ""

    @MainActor
    func load() async {
        guard let conv, releases.isEmpty,
              let out = try? await Conv.run(conv, ["changelog", "--json"]),
              let r = out.decode(Response.self) else { return }
        releases = r.results ?? []
        installed = r.installed ?? ""
    }
}

struct ChangelogView: View {
    let changelog: ChangelogModel

    var body: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 28) {
                ForEach(changelog.releases) { r in
                    VStack(alignment: .leading, spacing: 8) {
                        HStack(alignment: .firstTextBaseline) {
                            Text(r.version).font(.title2.weight(.semibold))
                            if r.version == changelog.installed {
                                Text("Installed").font(.caption.weight(.medium))
                                    .padding(.horizontal, 6).padding(.vertical, 2)
                                    .background(.tint.opacity(0.15), in: Capsule())
                            }
                            Spacer()
                            Text(r.date ?? "").foregroundStyle(.secondary)
                        }
                        MarkdownNotes(text: r.notes)
                    }
                }
            }
            .padding(24)
            .frame(maxWidth: 720, alignment: .leading)
            .frame(maxWidth: .infinity)
        }
        .navigationTitle("What's New")
        .task { await changelog.load() }
    }
}

/// Enough Markdown for the changelog: ### headings, - bullets with wrapped lines,
/// and inline bold and code.
struct MarkdownNotes: View {
    let text: String

    private enum Block: Hashable { case heading(String), bullet(String), para(String) }

    private var blocks: [Block] {
        var out: [Block] = []
        var current: Block?
        func flush() { if let c = current { out.append(c) }; current = nil }
        for raw in text.split(separator: "\n", omittingEmptySubsequences: false) {
            let line = raw.trimmingCharacters(in: .whitespaces)
            if line.isEmpty { flush(); continue }
            if line.hasPrefix("#") {
                flush()
                out.append(.heading(line.drop { $0 == "#" }.trimmingCharacters(in: .whitespaces)))
            } else if line.hasPrefix("- ") {
                flush()
                current = .bullet(String(line.dropFirst(2)))
            } else {
                switch current {
                case .bullet(let t): current = .bullet(t + " " + line)
                case .para(let t): current = .para(t + " " + line)
                default: current = .para(line)
                }
            }
        }
        flush()
        return out
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(blocks, id: \.self) { b in
                switch b {
                case .heading(let t):
                    Text(t).font(.headline).padding(.top, 4)
                case .bullet(let t):
                    HStack(alignment: .firstTextBaseline, spacing: 8) {
                        Text("•").foregroundStyle(.secondary)
                        Text(inline(t)).fixedSize(horizontal: false, vertical: true)
                    }
                case .para(let t):
                    Text(inline(t)).fixedSize(horizontal: false, vertical: true)
                }
            }
        }
        .textSelection(.enabled)
    }

    private func inline(_ s: String) -> AttributedString {
        (try? AttributedString(markdown: s, options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace)))
            ?? AttributedString(s)
    }
}
