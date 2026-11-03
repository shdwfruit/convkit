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
            Task { @MainActor in model.add(urls) }
        } else {
            pending += urls
        }
        showWindow()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }

    private func showWindow() {
        if window == nil {
            let host = NSHostingController(rootView: ContentView().environment(model))
            host.sceneBridgingOptions = [.toolbars, .title]
            let w = NSWindow(contentViewController: host)
            w.title = "ConvKit"
            w.styleMask = [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView]
            w.toolbarStyle = .unified
            w.setContentSize(NSSize(width: 900, height: 580))
            w.contentMinSize = NSSize(width: 820, height: 520)
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
                case "format": model.format = kv[1]
                case "maxSize": model.maxSize = kv[1]
                case "resize": model.resize = kv[1]
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
            var dump = "conv: \(model.convURL?.path ?? "-") \(model.convVersion)\ntargets: \(model.targets)\n"
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
