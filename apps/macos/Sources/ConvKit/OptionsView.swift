import AppKit
import SwiftUI

struct OptionsView: View {
    @Environment(AppModel.self) private var model
    @State private var showCommand = false

    var body: some View {
        @Bindable var model = model
        VStack(spacing: 0) {
            Form {
                Section {
                    Picker("Convert to", selection: $model.format) {
                        Text("Same format").tag("same")
                        if !model.targets.isEmpty { Divider() }
                        ForEach(model.targets, id: \.self) { ext in
                            Text(ext.uppercased()).tag(ext)
                        }
                    }
                    .disabled(model.files.isEmpty)
                    if !model.ignoredOptions.isEmpty && !model.files.isEmpty {
                        Label("\(model.ignoredOptions.joined(separator: ", ")) won't be used for \(model.format == "same" ? "this conversion" : model.format.uppercased()).",
                              systemImage: "info.circle")
                            .font(.caption)
                            .foregroundStyle(.orange)
                    }
                    ForEach(model.targetNotes, id: \.self) { note in
                        Text(note).font(.caption).foregroundStyle(.secondary)
                    }
                }

                Section("Size") {
                    PresetField(
                        "Keep under", value: $model.maxSize,
                        presets: [("No limit", ""), ("10 MB", "10mb"), ("For email (18 MB)", "18mb"),
                                  ("25 MB", "25mb")],
                        placeholder: "50 MB"
                    )
                    .applies(model.applies("--max-size"))
                    PresetField(
                        "Resize", value: $model.resize,
                        presets: [("Original size", ""), ("Half (50%)", "50%"), ("Quarter (25%)", "25%"),
                                  ("1920 wide", "1920x"), ("1280 wide", "1280x"), ("640 wide", "640x")],
                        placeholder: "1600x900"
                    )
                    .applies(model.applies("--resize"))
                    if !model.resize.isEmpty && model.applies("--upscale") {
                        Toggle("Allow enlarging a smaller file", isOn: $model.upscale)
                            .help("Enlarging adds no detail and makes a larger file. conv warns when it does.")
                    }
                    Toggle("Set quality", isOn: $model.useQuality)
                        .applies(model.applies("--quality"))
                    if model.useQuality && model.applies("--quality") {
                        Slider(value: $model.quality, in: 30...100, step: 1) {
                            Text("Quality \(Int(model.quality))")
                        }
                    }
                    PresetField(
                        "Colors", value: $model.colors,
                        presets: [("All", ""), ("256", "256"), ("64", "64"), ("16", "16")],
                        placeholder: "2 to 256"
                    )
                    .applies(model.applies("--colors"))
                }

                Section("Video") {
                    PresetField(
                        "Frame rate", value: $model.fps,
                        presets: [("As is", ""), ("Up to 60", "60"), ("Up to 30", "30"),
                                  ("Up to 24", "24"), ("Up to 15", "15")],
                        placeholder: "12"
                    )
                    .applies(model.applies("--fps"))
                    PresetField(
                        "Quality (CRF)", value: $model.crf,
                        presets: [("Default", ""), ("Near lossless (16)", "16"), ("High (20)", "20"),
                                  ("Smaller (28)", "28"), ("Smallest (34)", "34")],
                        placeholder: "0 to 51, lower is better"
                    )
                    .applies(model.applies("--crf"))
                }

                Section("Time") {
                    TextField("Start", text: $model.start, prompt: Text("0:00"))
                        .applies(model.applies("--start"))
                    TextField("End", text: $model.end, prompt: Text("end"))
                        .applies(model.applies("--end"))
                    TextField("Or keep", text: $model.duration, prompt: Text("a length, like 30s"))
                        .applies(model.applies("--duration"))
                    PresetField(
                        "Speed", value: $model.speed,
                        presets: [("Normal", ""), ("Half (0.5×)", "0.5"), ("1.5×", "1.5"),
                                  ("Double (2×)", "2"), ("4×", "4")],
                        placeholder: "3"
                    )
                    .applies(model.applies("--speed"))
                    if !model.speed.isEmpty && model.applies("--shift-pitch") {
                        Toggle("Let the pitch change too", isOn: $model.shiftPitch)
                    }
                }

                Section("Turn") {
                    Picker("Turn", selection: $model.rotate) {
                        Text("Don't turn").tag("")
                        Text("Right (90° clockwise)").tag("right")
                        Text("Left (90° counterclockwise)").tag("left")
                        Text("Upside down (180°)").tag("180")
                    }
                    .applies(model.applies("--rotate"))
                }

                Section("Privacy") {
                    Toggle("Remove location and other metadata", isOn: $model.strip)
                        .applies(model.applies("--strip-metadata"))
                }

                Section("Save in") {
                    Picker("Save in", selection: Binding(
                        get: { model.outputFolder == nil ? 0 : 1 },
                        set: { if $0 == 0 { model.outputFolder = nil } else { chooseFolder() } }
                    )) {
                        Text("Next to the original").tag(0)
                        Text(model.outputFolder?.lastPathComponent ?? "Choose…").tag(1)
                    }
                    .labelsHidden()
                    .pickerStyle(.radioGroup)
                    if model.outputFolder != nil {
                        Button("Change Folder…") { chooseFolder() }
                    }
                    if model.files.contains(where: \.pasted) && model.outputFolder == nil {
                        Text("A pasted picture has no folder of its own, so its result goes to Downloads.")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                    Toggle("Copy the result to the clipboard", isOn: $model.copyResult)
                        .help("A picture is copied as itself, a text file as its text, anything else as the file.")
                }
            }
            .formStyle(.grouped)

            Divider()
            footer
        }
    }

    private var footer: some View {
        VStack(alignment: .leading, spacing: 6) {
            DisclosureGroup("Command", isExpanded: $showCommand) {
                Text(model.commandPreview.isEmpty ? "Add a file to see it." : model.commandPreview)
                    .font(.system(.caption, design: .monospaced))
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.top, 4)
            }
            .font(.callout)
            if !model.convVersion.isEmpty {
                Text(model.convVersion).font(.caption2).foregroundStyle(.tertiary)
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
        if panel.runModal() == .OK, let url = panel.url { model.outputFolder = url }
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

private struct AppliesModifier: ViewModifier {
    let applies: Bool
    func body(content: Content) -> some View {
        content
            .disabled(!applies)
            .help(applies ? "" : "Doesn't apply to this conversion")
    }
}

extension View {
    /// Greys out a control conv would ignore for the chosen target.
    func applies(_ yes: Bool) -> some View { modifier(AppliesModifier(applies: yes)) }
}
