import AppKit
import SwiftUI

@main
struct ConvKitApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate

    var body: some Scene {
        Settings {
            SettingsView()
                .environment(delegate.model)
        }
        .commands {
            CommandGroup(replacing: .newItem) {
                Button("Add Files…") { pickFiles(delegate.model) }
                    .keyboardShortcut("o")
            }
        }
    }
}

/// Owns the main window. AppKit makes it directly because a SwiftUI `Window`
/// scene isn't created when the app is launched by opening files.
final class AppDelegate: NSObject, NSApplicationDelegate {
    let model = AppModel()
    private var window: NSWindow?
    private var pending: [URL] = []
    private var connected = false

    func applicationDidFinishLaunching(_ notification: Notification) {
        // Needed when started from a terminal rather than as an .app.
        NSApp.setActivationPolicy(.regular)
        showWindow()
        NSApp.activate()

        // `ConvKit file…` from a terminal.
        let argv = CommandLine.arguments.dropFirst()
            .filter { !$0.hasPrefix("-") && FileManager.default.fileExists(atPath: $0) }
            .map { URL(fileURLWithPath: $0) }
        pending += argv

        Task { @MainActor in
            await model.connect()
            connected = true
            model.add(pending)
            pending = []
        }

        if let path = ProcessInfo.processInfo.environment["CONVKIT_STATE"] {
            snapshot(to: path)
        }
    }

    /// Finder's Open With, or files dropped on the Dock icon.
    func application(_ application: NSApplication, open urls: [URL]) {
        if connected {
            Task { @MainActor in model.open(urls) }
        } else {
            pending += urls
        }
        showWindow()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }

    private func showWindow() {
        if window == nil {
            let host = NSHostingController(rootView: RootView().environment(model))
            host.sceneBridgingOptions = [.toolbars, .title]
            let w = NSWindow(contentViewController: host)
            w.title = "ConvKit"
            w.styleMask = [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView]
            w.toolbarStyle = .unified
            w.setContentSize(NSSize(width: 1080, height: 640))
            w.contentMinSize = NSSize(width: 960, height: 540)
            w.setFrameAutosaveName("ConvKitMain")
            w.isReleasedWhenClosed = false
            w.center()
            window = w
        }
        window?.makeKeyAndOrderFront(nil)
    }

    /// Dev aid for testing without clicking: CONVKIT_STATE=/tmp/state.txt writes the
    /// model's state there and quits. CONVKIT_SET="format=gif speed=2" presets options,
    /// CONVKIT_CONVERT=1 presses Convert, CONVKIT_WAIT sets the seconds before the dump.
    private func snapshot(to path: String) {
        let env = ProcessInfo.processInfo.environment
        DispatchQueue.main.asyncAfter(deadline: .now() + 2) { [self] in
            for pair in (env["CONVKIT_SET"] ?? "").split(separator: " ") {
                let kv = pair.split(separator: "=", maxSplits: 1).map(String.init)
                guard kv.count == 2 else { continue }
                switch kv[0] {
                case "tool": model.tool = Tool(rawValue: kv[1]) ?? .convert
                case "trimFile": model.trim.load(URL(fileURLWithPath: kv[1]))
                case "trimClip":
                    let parts = kv[1].split(separator: ",")
                    let t = parts.prefix(2).compactMap { Double($0) }
                    if t.count == 2 {
                        model.trim.clips.append(.init(start: t[0], end: t[1], keep: parts.count > 2 ? .sound : .both))
                    }
                case "trimWrite": model.trim.writeAll()
                case "mergeAdd": model.merge.add(kv[1].split(separator: ",").map { URL(fileURLWithPath: String($0)) })
                case "mergeRun":
                    // Bypasses the save panel: merges into the path given.
                    let args = ["merge", "--json", "--no-install"] + model.merge.items.map(\.url.path) + [kv[1]]
                    Task { if let c = model.convURL, let out = try? await Conv.run(c, args) {
                        try? out.stdout.write(to: URL(fileURLWithPath: kv[1] + ".json")) } }
                case "splitFile": model.split.load(URL(fileURLWithPath: kv[1]))
                case "splitRanges": model.split.ranges = kv[1].replacingOccurrences(of: "_", with: " "); model.split.changed()
                case "splitRun": model.split.split()
                case "scanDir": Task { await model.scan.scan(URL(fileURLWithPath: kv[1])) }
                case "qrText": model.qr.text = kv[1]; model.qr.changed()
                case "asciiInput": model.ascii.input = URL(fileURLWithPath: kv[1]); model.ascii.changed()
                case "asciiStyle": model.ascii.style = kv[1]; model.ascii.changed()
                case "asciiWidth": model.ascii.width = Double(kv[1]) ?? 100; model.ascii.changed()
                case "rotate": model.rotate = kv[1]
                case "copy": model.copyResult = kv[1] == "1"
                case "paste": Task { await model.paste() }
                case "format": model.format = kv[1]
                case "maxSize": model.maxSize = kv[1]
                case "resize": model.resize = kv[1]
                case "upscale": model.upscale = kv[1] == "1"
                case "colors": model.colors = kv[1]
                case "fps": model.fps = kv[1]
                case "crf": model.crf = kv[1]
                case "duration": model.duration = kv[1]
                case "speed": model.speed = kv[1]
                case "start": model.start = kv[1]
                case "end": model.end = kv[1]
                case "strip": model.strip = kv[1] == "1"
                case "quality": model.useQuality = true; model.quality = Double(kv[1]) ?? 92
                default: break
                }
            }
            model.optionsChanged()
        }
        if env["CONVKIT_CONVERT"] == "1" {
            DispatchQueue.main.asyncAfter(deadline: .now() + 4) { [self] in model.convertAll() }
        }
        let wait = Double(env["CONVKIT_WAIT"] ?? "") ?? 6
        DispatchQueue.main.asyncAfter(deadline: .now() + wait) { [self] in
            var dump = "conv: \(model.convURL?.path ?? "-") \(model.convVersion)\ntools: \(model.availableTools.map(\.rawValue)) showing \(model.tool.rawValue)\ntargets: \(model.targets)\n"
            dump += "clipboard: note=\(model.clipboardNote ?? "-") pasteProblem=\(model.pasteProblem ?? "-")\n"
            dump += "merge: \(model.merge.items.map { "\($0.url.lastPathComponent):\($0.pages ?? -1)" }) warnings=\(model.merge.warnings) problem=\(model.merge.problem ?? "-")\n"
            dump += "split: pages=\(model.split.pages ?? -1) planned=\(model.split.planned.map { "\(($0.path as NSString).lastPathComponent)\($0.pages ?? [])" }) written=\(model.split.written.map(\.lastPathComponent)) problem=\(model.split.problem ?? "-") exists=\(model.split.exists) warnings=\(model.split.warnings)\n"
            dump += "trim: dur=\(model.trim.duration) video=\(model.trim.hasVideo) targets=\(model.trim.targets) clips=\(model.trim.clips.map { "\($0.start)-\($0.end) \($0.status)" })\n"
            dump += "scan: \(model.scan.entries.map { "\($0.name):\($0.targets?.count ?? 0)" }) problem=\(model.scan.problem ?? "-")\n"
            dump += "backends: \(model.backends.doctor.map { "\($0.backend)=\($0.found)" }) outdated=\(model.backends.outdated) conv=\(model.backends.me?.update_hint ?? "-")\n"
            dump += "formats: \(model.formats.formats.count) selected=\(model.formats.selected ?? "-") targets=\(model.formats.detail?.targets?.count ?? 0)\n"
            dump += "changelog: \(model.changelog.releases.map(\.version)) installed=\(model.changelog.installed)\n"
            dump += "qr: preview=\(model.qr.preview?.size as Any) info=\(model.qr.info) problem=\(model.qr.problem ?? "-")\n"
            dump += "ascii: problem=\(model.ascii.problem ?? "-") warnings=\(model.ascii.warnings) art:\n\(model.ascii.art.split(separator: "\n").prefix(6).joined(separator: "\n"))\n"
            for f in model.files {
                dump += "\(f.url.lastPathComponent) -> \(f.plannedOutput?.lastPathComponent ?? "-") problem=\(f.problem ?? "-") warnings=\(f.warnings) status=\(f.status)\n"
            }
            try? dump.write(toFile: path, atomically: true, encoding: .utf8)
            NSApp.terminate(nil)
        }
    }
}

@MainActor
func pickFiles(_ model: AppModel) {
    let panel = NSOpenPanel()
    panel.allowsMultipleSelection = true
    panel.canChooseDirectories = true
    panel.canChooseFiles = true
    panel.prompt = "Add"
    if panel.runModal() == .OK { model.add(panel.urls) }
}

struct SettingsView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
        Form {
            TextField("conv path", text: $model.convOverride, prompt: Text("Found automatically"))
            LabeledContent("Using") {
                Text(model.convURL?.path ?? "Not found").textSelection(.enabled)
            }
            if !model.convVersion.isEmpty {
                LabeledContent("Version", value: model.convVersion)
            }
        }
        .formStyle(.grouped)
        .frame(width: 460)
    }
}
