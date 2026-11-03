import SwiftUI

/// One entry in the sidebar. A tool only shows when the conv in use has its subcommand,
/// so an older conv hides what it can't do.
enum Tool: String, CaseIterable, Identifiable {
    case convert, trim, scan
    case qr, ascii
    case backends, formats, changelog

    var id: String { rawValue }

    var title: String {
        switch self {
        case .convert: "Convert"
        case .trim: "Trim"
        case .scan: "Scan Folder"
        case .qr: "QR Code"
        case .ascii: "ASCII Art"
        case .backends: "Backends"
        case .formats: "Formats"
        case .changelog: "What's New"
        }
    }

    var systemImage: String {
        switch self {
        case .convert: "arrow.triangle.2.circlepath"
        case .trim: "scissors"
        case .scan: "folder.badge.questionmark"
        case .qr: "qrcode"
        case .ascii: "textformat.abc"
        case .backends: "shippingbox"
        case .formats: "list.bullet.rectangle"
        case .changelog: "sparkles"
        }
    }

    var subtitle: String {
        switch self {
        case .convert: "Change format, size, length or speed"
        case .trim: "Cut clips out of a video or audio file"
        case .scan: "See what a folder's files could become"
        case .qr: "A code for a link or any text"
        case .ascii: "Turn a picture into text"
        case .backends: "The programs conv uses, and installing them"
        case .formats: "Every format and what it turns into"
        case .changelog: "What each version of conv added"
        }
    }

    /// The conv subcommand this tool needs, if any.
    var subcommand: String? {
        switch self {
        case .convert: nil
        case .trim: "trim"
        case .scan: "scan"
        case .qr: "qr"
        case .ascii: "ascii"
        case .backends: "doctor"
        case .formats: "capabilities"
        case .changelog: "changelog"
        }
    }

    enum Section: String, CaseIterable {
        case files = "Files"
        case make = "Make"
        case conv = "conv"
    }

    var section: Section {
        switch self {
        case .convert, .trim, .scan: .files
        case .qr, .ascii: .make
        case .backends, .formats, .changelog: .conv
        }
    }
}

struct RootView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
        NavigationSplitView {
            List(selection: $model.tool) {
                ForEach(Tool.Section.allCases, id: \.self) { section in
                    let tools = model.availableTools.filter { $0.section == section }
                    if !tools.isEmpty {
                        Section(section.rawValue) {
                            ForEach(tools) { tool in
                                Label(tool.title, systemImage: tool.systemImage)
                                    .help(tool.subtitle)
                                    .tag(tool)
                            }
                        }
                    }
                }
            }
            .navigationSplitViewColumnWidth(min: 170, ideal: 190, max: 240)
        } detail: {
            switch model.tool {
            case .convert: ConvertView()
            case .qr: QRView(qr: model.qr)
            case .ascii: AsciiView(ascii: model.ascii)
            case .trim: TrimView(trim: model.trim)
            case .scan: ScanView(scan: model.scan) { urls in
                    model.tool = .convert
                    model.add(urls)
                }
            case .backends: BackendsView(backends: model.backends)
            case .formats: FormatsView(formats: model.formats)
            case .changelog: ChangelogView(changelog: model.changelog)
            }
        }
    }
}

/// Save-panel helper shared by the tools that make one file.
@MainActor
func askWhereToSave(name: String, types: [String]) -> URL? {
    let panel = NSSavePanel()
    panel.nameFieldStringValue = name
    panel.allowedContentTypes = types.compactMap { UTType(filenameExtension: $0) }
    panel.allowsOtherFileTypes = false
    panel.isExtensionHidden = false
    return panel.runModal() == .OK ? panel.url : nil
}

import UniformTypeIdentifiers
