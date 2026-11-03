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
                    Toggle("Set quality", isOn: $model.useQuality)
                        .applies(model.applies("--quality"))
                    if model.useQuality && model.applies("--quality") {
                        Slider(value: $model.quality, in: 30...100, step: 1) {
                            Text("Quality \(Int(model.quality))")
                        }
                    }
                }

                Section("Time") {
                    TextField("Start", text: $model.start, prompt: Text("0:00"))
                        .applies(model.applies("--start"))
                    TextField("End", text: $model.end, prompt: Text("end"))
                        .applies(model.applies("--end"))
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
