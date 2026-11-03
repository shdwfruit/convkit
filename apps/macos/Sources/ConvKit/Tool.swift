import SwiftUI

/// One entry in the sidebar. A tool only shows when the conv in use has its subcommand,
/// so an older conv hides what it can't do.
enum Tool: String, CaseIterable, Identifiable {
    case convert
    case qr
    case ascii

    var id: String { rawValue }

    var title: String {
        switch self {
        case .convert: "Convert"
        case .qr: "QR Code"
        case .ascii: "ASCII Art"
        }
    }

    var systemImage: String {
        switch self {
        case .convert: "arrow.triangle.2.circlepath"
        case .qr: "qrcode"
        case .ascii: "textformat.abc"
        }
    }

    var subtitle: String {
        switch self {
        case .convert: "Change format, size, length or speed"
        case .qr: "A code for a link or any text"
        case .ascii: "Turn a picture into text"
        }
    }

    /// The conv subcommand this tool needs, if any.
    var subcommand: String? {
        switch self {
        case .convert: nil
        case .qr: "qr"
        case .ascii: "ascii"
        }
    }

    enum Section: String, CaseIterable {
        case files = "Files"
        case make = "Make"
    }

    var section: Section {
        switch self {
        case .convert: .files
        case .qr, .ascii: .make
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
