import AppKit
import SwiftUI
import UniformTypeIdentifiers

/// A convert-style tool's form. It shows only the tool's own knobs, and greys out any
/// that conv wouldn't use for the files in the list, saying why.
struct OptionsView: View {
    @Bindable var job: ConvertJob
    let convVersion: String
    /// Opens the files and settings in Convert Anything; nil there.
    var moreOptions: (() -> Void)?
    @State private var showCommand = false

    private func shows(_ c: Control) -> Bool { job.preset.controls.contains(c) }

    var body: some View {
        VStack(spacing: 0) {
            Form {
                formatSection
                if shows(.compress) || shows(.maxSize) || shows(.resize) || shows(.quality) || shows(.colors) { sizeSection }
                if shows(.pages) || shows(.dpi) { pagesSection }
                if shows(.fps) || shows(.crf) { videoSection }
                if shows(.rotate) || shows(.flip) || shows(.crop) { pictureSection }
                if shows(.time) || shows(.every) || shows(.speed) { timeSection }
                if shows(.denoise) || shows(.audio) { soundSection }
                if shows(.subtitles) { subtitlesSection }
                if shows(.lock) || shows(.unlock) || job.preset.alwaysUnlock { passwordSection }
                if shows(.strip) || job.preset.alwaysStrip { privacySection }
                saveSection
                if let moreOptions {
                    Section {
                        Button("More Options…", action: moreOptions)
                            .disabled(job.fitting.isEmpty || job.isRunning)
                    } footer: {
                        Text("Opens these files in Convert Anything, with every option conv has.")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }
            }
            .formStyle(.grouped)

            Divider()
            footer
        }
    }

    // MARK: Sections

    @ViewBuilder
    private var formatSection: some View {
        Section {
            if let fixed = job.preset.fixedTarget {
                LabeledContent("Makes", value: fixed.uppercased())
            } else if case .keep = job.preset.target {
                LabeledContent("Saves as", value: keptName)
            } else {
                Picker(job.allowsSame ? "Save as" : "Convert to", selection: Binding(
                    get: { job.format },
                    set: { job.choose(format: $0) }
                )) {
                    if job.format.isEmpty {
                        Text(job.fitting.isEmpty ? "Add a file first" : "Nothing fits").tag("")
                    }
                    if job.allowsSame { Text("Same format").tag("same") }
                    if job.allowsSame && !job.targets.isEmpty { Divider() }
                    ForEach(job.targets, id: \.self) { ext in
                        Text(ext.uppercased()).tag(ext)
                    }
                }
                .disabled(job.fitting.isEmpty)
            }
            if job.isGeneral && job.fitting.isEmpty {
                Text("Add a file to see which options fit it.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            if !job.ignoredOptions.isEmpty {
                Label("\(job.ignoredOptions.joined(separator: ", ")) won't be used for \(job.format == "same" ? "these files" : job.format.uppercased()).",
                      systemImage: "info.circle")
                    .font(.caption)
                    .foregroundStyle(.orange)
            }
            ForEach(job.targetNotes, id: \.self) { note in
                Text(note).font(.caption).foregroundStyle(.secondary)
            }
        }
    }

    /// "PDF" for a list of PDFs, "Same format" for a mix.
    private var keptName: String {
        let names = Set(job.fitting.map { job.caps.name($0.ext).uppercased() })
        return names.count == 1 ? names.first! : "Same format"
    }

    private var sizeSection: some View {
        Section {
            if shows(.compress) {
                Toggle("Shrink big pictures, keep the look", isOn: $job.compress)
                    .help("Pictures drawn above 225 dpi go to 150 dpi, and high-quality JPEGs are saved again. Text, links, bookmarks and forms stay as they are.")
                    .why(job.reason("--compress"))
            }
            if shows(.maxSize) {
                PresetField(
                    "Keep under", value: $job.maxSize,
                    presets: [("No limit", ""), ("8 MB", "8mb"), ("10 MB", "10mb"), ("For email (18 MB)", "18mb"),
                              ("25 MB", "25mb"), ("50 MB", "50mb")],
                    placeholder: "50 MB"
                )
                .why(job.reason("--max-size"))
            }
            if shows(.resize) {
                PresetField(
                    "Resize", value: $job.resize,
                    presets: [("Original size", ""), ("Half (50%)", "50%"), ("Quarter (25%)", "25%"),
                              ("1920 wide", "1920x"), ("1280 wide", "1280x"), ("640 wide", "640x"),
                              ("480 wide", "480x")],
                    placeholder: "1600x900"
                )
                .why(job.reason("--resize"))
                if !job.resize.isEmpty && job.enabled("--resize") && job.applies("--upscale") {
                    Toggle("Allow enlarging a smaller file", isOn: $job.upscale)
                        .help("Enlarging adds no detail and makes a larger file. conv warns when it does.")
                }
            }
            if shows(.quality) {
                Toggle("Set quality", isOn: $job.useQuality)
                    .why(job.reason("--quality"))
                if job.useQuality && job.enabled("--quality") {
                    Slider(value: $job.quality, in: 30...100, step: 1) {
                        Text("Quality \(Int(job.quality))")
                    }
                }
            }
            if shows(.colors) {
                PresetField(
                    "Colors", value: $job.colors,
                    presets: [("All", ""), ("256", "256"), ("64", "64"), ("16", "16")],
                    placeholder: "2 to 256"
                )
                .why(job.reason("--colors"))
            }
        } header: {
            Text("Size")
        } footer: {
            VStack(alignment: .leading, spacing: 4) {
                sectionReason([.compress, .maxSize, .resize, .quality, .colors])
                if shows(.compress) && job.applies("--compress") {
                    Text("The result is never bigger than the PDF. A size to keep under makes conv try lower resolutions until it fits, and it asks before going below 120 dpi.")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }
        }
    }

    private var pagesSection: some View {
        Section {
            if shows(.pages) {
                TextField("Pages", text: $job.pdfPages, prompt: Text("Every page"))
                    .why(job.reason("--pages"))
            }
            if shows(.dpi) {
                PresetField(
                    "Resolution", value: $job.dpi,
                    presets: [("150 dpi", ""), ("72 dpi, for screens", "72"), ("300 dpi, for print", "300"),
                              ("600 dpi", "600")],
                    placeholder: "36 to 1200"
                )
                .why(job.reason("--dpi"))
            }
        } header: {
            Text("Pages")
        } footer: {
            VStack(alignment: .leading, spacing: 4) {
                sectionReason([.pages, .dpi])
                if job.applies("--pages") {
                    Text("Like 1-3,7. z is the last page. One page makes one file; more go into a folder.")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }
        }
    }

    private var videoSection: some View {
        Section {
            if shows(.fps) {
                PresetField(
                    "Frame rate", value: $job.fps,
                    presets: [("As is", ""), ("Up to 60", "60"), ("Up to 30", "30"),
                              ("Up to 24", "24"), ("Up to 15", "15"), ("Up to 10", "10")],
                    placeholder: "12"
                )
                .why(job.reason("--fps"))
            }
            if shows(.crf) {
                PresetField(
                    "Quality (CRF)", value: $job.crf,
                    presets: [("Default", ""), ("Near lossless (16)", "16"), ("High (20)", "20"),
                              ("Smaller (28)", "28"), ("Smallest (34)", "34")],
                    placeholder: "0 to 51, lower is better"
                )
                .why(job.reason("--crf"))
            }
        } header: {
            Text("Video")
        } footer: {
            sectionReason([.fps, .crf])
        }
    }

    private var timeSection: some View {
        Section {
            if shows(.time) {
                TextField("Start", text: $job.start, prompt: Text("0:00"))
                    .why(job.reason("--start"))
                TextField("End", text: $job.end, prompt: Text("the end"))
                    .why(job.reason("--end"))
                TextField("Or keep", text: $job.duration, prompt: Text("a length, like 30s"))
                    .why(job.reason("--duration"))
            }
            if shows(.every) {
                PresetField(
                    "A still every", value: $job.every,
                    presets: [("Not set", ""), ("Second", "1s"), ("2 seconds", "2s"), ("5 seconds", "5s"),
                              ("10 seconds", "10s"), ("Frame", "frame")],
                    placeholder: "0.5 or 1:00"
                )
                .why(job.reason("--every"))
            }
            if shows(.speed) {
                PresetField(
                    "Speed", value: $job.speed,
                    presets: [("Normal", ""), ("Half (0.5×)", "0.5"), ("1.5×", "1.5"),
                              ("Double (2×)", "2"), ("4×", "4")],
                    placeholder: "3"
                )
                .why(job.reason("--speed"))
                if !job.speed.isEmpty && job.enabled("--speed") && job.applies("--shift-pitch") {
                    Toggle("Let the pitch change too", isOn: $job.shiftPitch)
                }
            }
        } header: {
            Text("Time")
        } footer: {
            VStack(alignment: .leading, spacing: 4) {
                sectionReason([.time, .every, .speed])
                if shows(.time) && job.applies("--start") {
                    Text("Seconds (90), minutes (1:30) or hours (1:02:03). A minus counts from the end: -30 is the last 30 seconds.")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }
        }
    }

    private var pictureSection: some View {
        Section {
            if shows(.rotate) {
                Picker("Turn", selection: $job.rotate) {
                    Text("Don't turn").tag("")
                    Text("Right (90° clockwise)").tag("right")
                    Text("Left (90° counterclockwise)").tag("left")
                    Text("Upside down (180°)").tag("180")
                }
                .why(job.reason("--rotate"))
            }
            if shows(.flip) {
                Picker("Flip", selection: $job.flip) {
                    Text("Don't flip").tag("")
                    Text("Mirror, left to right").tag("h")
                    Text("Top to bottom").tag("v")
                }
                .why(job.reason("--flip"))
            }
            if shows(.crop) {
                PresetField(
                    "Crop", value: $job.crop,
                    presets: [("Don't crop", ""), ("Square (1:1)", "1:1"), ("Tall, for Stories (9:16)", "9:16"),
                              ("Wide (16:9)", "16:9"), ("Portrait (4:5)", "4:5"), ("Classic (4:3)", "4:3"),
                              ("Cut away bars or a border", "auto")],
                    placeholder: "1280x720+0+140"
                )
                .why(job.reason("--crop"))
            }
        } header: {
            Text("Picture")
        } footer: {
            VStack(alignment: .leading, spacing: 4) {
                sectionReason([.rotate, .flip, .crop])
                if shows(.crop) && job.applies("--crop") {
                    Text("A shape keeps the largest box of it in the middle. WxH is a box in the middle, WxH+X+Y one from the top left.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                if shows(.rotate) && shows(.flip) && job.applies("--flip") {
                    Text("With both, the turn comes first.").font(.caption).foregroundStyle(.secondary)
                }
            }
        }
    }

    private var soundSection: some View {
        Section {
            if shows(.denoise) {
                Picker("Background noise", selection: $job.denoise) {
                    if !job.preset.startsOn.contains(.denoise) { Text("Leave it").tag("") }
                    Text("Take it out").tag("normal")
                    Text("Take more out (strong)").tag("strong")
                }
                .why(job.reason("--denoise"))
                .help("Takes background noise and mains hum out of a voice. Strong takes more off a very noisy recording, at some cost to the voice.")
            }
            if shows(.audio) {
                FilePickRow(title: "New sound", file: $job.audioFile, types: [.audio, .audiovisualContent])
                    .why(job.reason("--audio"))
                if job.audioFile != nil && job.enabled("--audio") {
                    Toggle("Mix it under the video's own sound", isOn: $job.mix)
                        .why(job.reason("--mix"))
                    if job.mix {
                        PresetField(
                            "How loud", value: $job.mixLevel,
                            presets: [("A quarter as loud", ""), ("Half as loud", "50%"), ("As loud", "100%")],
                            placeholder: "40%"
                        )
                    }
                }
            }
        } header: {
            Text("Sound")
        } footer: {
            VStack(alignment: .leading, spacing: 4) {
                sectionReason([.denoise, .audio])
                if shows(.audio) && job.applies("--audio") {
                    Text("The video's length decides: a longer sound stops at its end, and a shorter one leaves the rest silent. Both are measured first, so the level holds.")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }
        }
    }

    private var subtitlesSection: some View {
        Section {
            FilePickRow(title: "Subtitles", file: $job.subtitlesFile,
                        types: ["srt", "vtt", "ass"].compactMap { UTType(filenameExtension: $0) })
                .why(job.reason("--subtitles"))
        } header: {
            Text("Subtitles")
        } footer: {
            VStack(alignment: .leading, spacing: 4) {
                sectionReason([.subtitles])
                if job.applies("--subtitles") {
                    Text("An .srt, .vtt or .ass file. Italics and bold are kept, and a cut or speed change moves them with the video.")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }
        }
    }

    @ViewBuilder
    private var passwordSection: some View {
        let locks = shows(.lock) && !job.preset.alwaysUnlock
        let unlocks = shows(.unlock) || job.preset.alwaysUnlock
        Section {
            if locks {
                if !job.preset.startsOn.contains(.lock) {
                    Toggle("Lock with a password", isOn: $job.lock)
                        .why(job.reason("--password"))
                }
                if job.lock {
                    SecureField("Password", text: $job.lockPassword)
                        .why(job.reason("--password"))
                    SecureField("Again", text: $job.lockAgain)
                        .why(job.reason("--password"))
                    Toggle("Allow printing", isOn: $job.allowPrint).why(job.reason("--restrict"))
                    Toggle("Allow copying text", isOn: $job.allowCopy).why(job.reason("--restrict"))
                    Toggle("Allow editing", isOn: $job.allowEdit).why(job.reason("--restrict"))
                }
            }
            if unlocks {
                if !job.preset.alwaysUnlock {
                    Toggle("Remove the password", isOn: $job.unlock)
                        .why(job.reason("--unlock"))
                }
                if job.unlock {
                    SecureField("Password", text: $job.openPassword, prompt: Text("The one that opens it"))
                        .why(job.reason("--unlock"))
                }
            }
        } header: {
            Text("Password")
        } footer: {
            VStack(alignment: .leading, spacing: 4) {
                sectionReason([.lock, .unlock])
                if locks && job.lock && job.applies("--password") {
                    Text("AES-256. The password goes to conv privately, never on a command line. PDF apps honour the restrictions, though nothing forces them to.")
                        .font(.caption).foregroundStyle(.secondary)
                } else if unlocks && job.unlock && job.applies("--unlock") {
                    Text("Writes a copy without the password and the print, copy and edit restrictions.")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }
        }
    }

    private var privacySection: some View {
        Section {
            if job.preset.alwaysStrip {
                Label("Removes location and other metadata", systemImage: "checkmark.circle")
                    .why(job.reason("--strip-metadata"))
            } else {
                Toggle("Remove location and other metadata", isOn: $job.strip)
                    .why(job.reason("--strip-metadata"))
            }
        } header: {
            Text("Privacy")
        } footer: {
            sectionReason([.strip])
        }
    }

    private var saveSection: some View {
        Section("Save in") {
            Picker("Save in", selection: Binding(
                get: { job.outputFolder == nil ? 0 : 1 },
                set: { if $0 == 0 { job.outputFolder = nil } else { chooseFolder() } }
            )) {
                Text("Next to the original").tag(0)
                Text(job.outputFolder?.lastPathComponent ?? "Choose…").tag(1)
            }
            .labelsHidden()
            .pickerStyle(.radioGroup)
            if job.outputFolder != nil {
                Button("Change Folder…") { chooseFolder() }
            }
            if job.files.contains(where: \.pasted) && job.outputFolder == nil {
                Text("A pasted picture has no folder of its own, so its result goes to Downloads.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Toggle("Copy the result to the clipboard", isOn: $job.copyResult)
                .help("A picture is copied as itself, a text file as its text, anything else as the file.")
        }
    }

    /// One line under a section whose every knob is greyed out, so the reason is visible
    /// without hovering.
    @ViewBuilder
    private func sectionReason(_ controls: [Control]) -> some View {
        let flags = controls.filter(shows).flatMap(\.flags)
        let reasons = flags.compactMap(job.reason)
        if !flags.isEmpty && reasons.count == flags.count, let first = reasons.first {
            Text(first).font(.caption).foregroundStyle(.secondary)
        }
    }

    private var footer: some View {
        VStack(alignment: .leading, spacing: 6) {
            DisclosureGroup("Command", isExpanded: $showCommand) {
                Text(job.commandPreview.isEmpty ? "Add a file to see it." : job.commandPreview)
                    .font(.system(.caption, design: .monospaced))
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.top, 4)
            }
            .font(.callout)
            if !convVersion.isEmpty {
                Text(convVersion).font(.caption2).foregroundStyle(.tertiary)
            }
        }
        .padding(12)
    }

    private func chooseFolder() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.prompt = "Save Here"
        if panel.runModal() == .OK, let url = panel.url { job.outputFolder = url }
    }
}

/// A popup of presets plus "Other…", which reveals a text field.
struct PresetField: View {
    let title: String
    @Binding var value: String
    let presets: [(String, String)]
    let placeholder: String
    @State private var custom = false

    init(_ title: String, value: Binding<String>, presets: [(String, String)], placeholder: String) {
        self.title = title
        self._value = value
        self.presets = presets
        self.placeholder = placeholder
    }

    private static let other = "\u{0}other"

    var body: some View {
        Picker(title, selection: Binding(
            get: { custom || !presets.contains { $0.1 == value } ? Self.other : value },
            set: { picked in
                if picked == Self.other {
                    custom = true
                    value = ""
                } else {
                    custom = false
                    value = picked
                }
            }
        )) {
            ForEach(presets, id: \.1) { Text($0.0).tag($0.1) }
            Divider()
            Text("Other…").tag(Self.other)
        }
        if custom || !presets.contains(where: { $0.1 == value }) {
            TextField(title, text: $value, prompt: Text(placeholder))
                .labelsHidden()
        }
    }
}

/// A file the option needs (a song, a subtitles file): its name, Choose… and Remove.
struct FilePickRow: View {
    let title: String
    @Binding var file: URL?
    let types: [UTType]

    var body: some View {
        LabeledContent(title) {
            HStack(spacing: 6) {
                if let file {
                    Text(file.lastPathComponent).lineLimit(1).truncationMode(.middle)
                        .foregroundStyle(.secondary)
                        .help(file.path)
                    Button("Remove") { self.file = nil }
                }
                Button(file == nil ? "Choose…" : "Change…") { choose() }
            }
        }
    }

    private func choose() {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = types
        if panel.runModal() == .OK, let url = panel.url { file = url }
    }
}

private struct WhyModifier: ViewModifier {
    let reason: String?
    func body(content: Content) -> some View {
        content
            .disabled(reason != nil)
            .help(reason ?? "")
    }
}

extension View {
    /// Greys out a control conv wouldn't use, with the reason on hover.
    func why(_ reason: String?) -> some View { modifier(WhyModifier(reason: reason)) }
}
