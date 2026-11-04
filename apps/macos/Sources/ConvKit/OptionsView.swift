import AppKit
import SwiftUI

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
                if shows(.maxSize) || shows(.resize) || shows(.quality) || shows(.colors) { sizeSection }
                if shows(.fps) || shows(.crf) { videoSection }
                if shows(.time) || shows(.speed) { timeSection }
                if shows(.rotate) { turnSection }
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

    private var sizeSection: some View {
        Section {
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
            sectionReason([.maxSize, .resize, .quality, .colors])
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
                sectionReason([.time, .speed])
                if shows(.time) && job.applies("--start") {
                    Text("Seconds (90), minutes (1:30) or hours (1:02:03). A minus counts from the end: -30 is the last 30 seconds.")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }
        }
    }

    private var turnSection: some View {
        Section {
            Picker("Turn", selection: $job.rotate) {
                Text("Don't turn").tag("")
                Text("Right (90° clockwise)").tag("right")
                Text("Left (90° counterclockwise)").tag("left")
                Text("Upside down (180°)").tag("180")
            }
            .why(job.reason("--rotate"))
        } header: {
            Text("Turn")
        } footer: {
            sectionReason([.rotate])
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
