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
                Button("Add Files…") {
                    if case .tool(let id) = delegate.model.page, let job = delegate.model.jobs[id] {
                        pickFiles(job)
                    } else {
                        pickFiles(delegate.model)
                    }
                }
                .keyboardShortcut("o")
            }
            CommandGroup(after: .sidebar) {
                Button("Home") { delegate.model.page = .home }
                    .keyboardShortcut("0", modifiers: .command)
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
            model.open(pending)
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
            w.setContentSize(NSSize(width: 1160, height: 720))
            w.contentMinSize = NSSize(width: 980, height: 560)
            w.setFrameAutosaveName("ConvKitMain")
            w.isReleasedWhenClosed = false
            w.center()
            window = w
        }
        window?.makeKeyAndOrderFront(nil)
    }

    /// Dev aid for testing without clicking: CONVKIT_STATE=/tmp/state.txt writes the
    /// model's state there and quits. CONVKIT_SET="page=compressVideo add=/a.mp4 maxSize=10mb"
    /// sets things up in order (each `add=` waits for the one before), CONVKIT_CONVERT=1
    /// presses the tool's main button, CONVKIT_WAIT sets the seconds before the dump.
    private func snapshot(to path: String) {
        let env = ProcessInfo.processInfo.environment
        Task { @MainActor [self] in
            while !connected { try? await Task.sleep(for: .milliseconds(100)) }
            for pair in (env["CONVKIT_SET"] ?? "").split(separator: " ") {
                let kv = pair.split(separator: "=", maxSplits: 1).map(String.init)
                guard kv.count == 2 else { continue }
                let v = kv[1].replacingOccurrences(of: "_", with: " ")
                let urls = kv[1].split(separator: ",").map { URL(fileURLWithPath: String($0)) }
                let job: ConvertJob? = { if case .tool(let id) = model.page { return model.jobs[id] }; return nil }()
                switch kv[0] {
                case "page": model.page = kv[1] == "home" ? .home : ToolID(rawValue: kv[1]).map(Page.tool) ?? .home
                case "search": model.search = v
                case "open": model.open(urls); try? await Task.sleep(for: .milliseconds(400))
                case "add":
                    if case .tool(let id) = model.page { model.open(id, with: urls) }
                    try? await Task.sleep(for: .milliseconds(600))
                case "pick":
                    // A suggestion on Home, as if clicked.
                    if let id = ToolID(rawValue: kv[1]) { model.open(id, with: model.dropped) }
                    try? await Task.sleep(for: .milliseconds(600))
                case "more": if let job { model.moreOptions(from: job) }; try? await Task.sleep(for: .milliseconds(600))
                case "trimClip":
                    let parts = kv[1].split(separator: ",")
                    let t = parts.prefix(2).compactMap { Double($0) }
                    if t.count == 2 {
                        model.trimVideo.clips.append(.init(start: t[0], end: t[1], keep: parts.count > 2 ? .sound : .both))
                    }
                case "trimWrite": model.trimVideo.writeAll()
                case "mergeRun":
                    // Bypasses the save panel: merges into the path given.
                    let m = model.page == .tool(.photosToPDF) ? model.photos : model.merge
                    let args = (m.mode == .pdfs ? ["merge"] : []) + ["--json", "--no-install"] + m.items.map(\.url.path) + [kv[1]]
                    if let c = model.convURL, let out = try? await Conv.run(c, args) {
                        try? out.stdout.write(to: URL(fileURLWithPath: kv[1] + ".json"))
                    }
                case "splitRanges": model.split.ranges = v; model.split.changed()
                case "splitRun": model.split.split()
                case "qrText": model.qr.text = v; model.qr.changed()
                case "asciiStyle": model.ascii.style = kv[1]; model.ascii.changed()
                case "asciiWidth": model.ascii.width = Double(kv[1]) ?? 100; model.ascii.changed()
                case "dataText": model.data.text = kv[1].replacingOccurrences(of: "\\n", with: "\n"); model.data.changed()
                case "dataFrom": model.data.from = kv[1]; model.data.changed()
                case "dataTo": model.data.to = kv[1] == "same" ? "" : kv[1]; model.data.changed()
                case "dataIndent": model.data.indent = kv[1]; model.data.changed()
                case "dataMinify": model.data.minify = kv[1] == "1"; model.data.changed()
                case "dataSort": model.data.sortKeys = kv[1] == "1"; model.data.changed()
                case "paste": await job?.paste()
                case "format": job?.choose(format: kv[1])
                case "rotate": job?.rotate = kv[1]
                case "copy": job?.copyResult = kv[1] == "1"
                case "maxSize": job?.maxSize = kv[1]
                case "resize": job?.resize = kv[1]
                case "upscale": job?.upscale = kv[1] == "1"
                case "colors": job?.colors = kv[1]
                case "fps": job?.fps = kv[1]
                case "crf": job?.crf = kv[1]
                case "duration": job?.duration = kv[1]
                case "speed": job?.speed = kv[1]
                case "start": job?.start = kv[1]
                case "end": job?.end = kv[1]
                case "strip": job?.strip = kv[1] == "1"
                case "quality": job?.useQuality = true; job?.quality = Double(kv[1]) ?? 92
                default: break
                }
                job?.optionsChanged()
            }
            if env["CONVKIT_CONVERT"] == "1" {
                try? await Task.sleep(for: .seconds(1.5))
                if case .tool(let id) = model.page { model.jobs[id]?.runAll() }
            }
            let wait = Double(env["CONVKIT_WAIT"] ?? "") ?? 4
            try? await Task.sleep(for: .seconds(wait))
            if let png = env["CONVKIT_RENDER"] { render(to: png) }
            try? dump().write(toFile: path, atomically: true, encoding: .utf8)
            NSApp.terminate(nil)
        }
    }

    /// Draws Home off screen; the window itself can't be captured without Screen Recording.
    @MainActor
    private func render(to path: String) {
        let dark = ProcessInfo.processInfo.environment["CONVKIT_DARK"] == "1"
        let view = HomeContent()
            .environment(model)
            .frame(width: CGFloat(Double(ProcessInfo.processInfo.environment["CONVKIT_WIDTH"] ?? "") ?? 900))
            .background(dark ? Color(white: 0.12) : Color(white: 0.98))
            .environment(\.colorScheme, dark ? .dark : .light)
        let r = ImageRenderer(content: view)
        r.scale = 2
        if let img = r.nsImage, let tiff = img.tiffRepresentation,
           let rep = NSBitmapImageRep(data: tiff), let data = rep.representation(using: .png, properties: [:]) {
            try? data.write(to: URL(fileURLWithPath: path))
        }
    }

    @MainActor
    private func dump() -> String {
        var d = "conv: \(model.convURL?.path ?? "-") \(model.convVersion) formats=\(model.caps.byExt.count)\n"
        d += "page: \(model.page) search=\(model.search)\n"
        for c in Category.allCases {
            let tools = model.tools(in: c)
            if !tools.isEmpty { d += "  \(c.title): \(tools.map(\.rawValue).joined(separator: " "))\n" }
        }
        d += "dropped: \(model.dropped.map(\.lastPathComponent)) suggestions: \(model.suggestions.map(\.rawValue))\n"
        d += "recent: \(model.recentTools.map(\.rawValue))\n"
        if case .tool(let id) = model.page {
            switch id.kind {
            case .convert:
                if let job = model.jobs[id] { d += dumpJob(job) }
            case .trim:
                let t = id == .trimAudio ? model.trimAudio : model.trimVideo
                d += "trim: dur=\(t.duration) video=\(t.hasVideo) targets=\(t.targets) clips=\(t.clips.map { "\($0.start)-\($0.end) \($0.status)" })\n"
            case .mergePDF, .photosToPDF:
                let m = id == .mergePDF ? model.merge : model.photos
                d += "merge(\(m.mode)): \(m.items.map { "\($0.url.lastPathComponent):\($0.pages ?? -1)" }) skipped=\(m.skipped) ready=\(m.ready) warnings=\(m.warnings) problem=\(m.problem ?? "-")\n"
            case .split:
                let s = model.split
                d += "split: pages=\(s.pages ?? -1) planned=\(s.planned.map { "\(($0.path as NSString).lastPathComponent)\($0.pages ?? [])" }) written=\(s.written.map(\.lastPathComponent)) problem=\(s.problem ?? "-") exists=\(s.exists)\n"
            case .scan:
                d += "scan: \(model.scan.entries.map { "\($0.name):\($0.targets?.count ?? 0)" }) problem=\(model.scan.problem ?? "-")\n"
            case .qr:
                d += "qr: preview=\(model.qr.preview?.size as Any) info=\(model.qr.info) problem=\(model.qr.problem ?? "-")\n"
            case .ascii:
                d += "ascii: input=\(model.ascii.input?.lastPathComponent ?? "-") problem=\(model.ascii.problem ?? "-") lines=\(model.ascii.art.split(separator: "\n").count)\n"
            case .data:
                let m = model.data
                d += "data: in=\(m.inputLanguage ?? "-") out=\(m.outLanguage ?? "-") args=\(m.args) minify=\(m.reason("minify") ?? "ok") indent=\(m.reason("indent") ?? "ok") tab=\(m.reason("tab") ?? "ok") keep=\(m.reason("keep") ?? "ok")\n"
                d += "  problem=\(m.problem.map { "\($0.message) | \($0.hint ?? "") | \($0.snippet ?? "")@\($0.caret ?? -1)" } ?? "-") notes=\(m.notes)\n"
                d += "  output:\n\(m.output.split(separator: "\n").prefix(8).map { "    " + $0 }.joined(separator: "\n"))\n"
            case .backends:
                d += "backends: \(model.backends.doctor.map { "\($0.backend)=\($0.found)" })\n"
            case .formats:
                d += "formats: \(model.formats.formats.count)\n"
            case .changelog:
                d += "changelog: \(model.changelog.releases.map(\.version))\n"
            }
        }
        return d
    }

    @MainActor
    private func dumpJob(_ job: ConvertJob) -> String {
        var d = "job \(job.tool.rawValue): format=\(job.format) targets=\(job.targets) ready=\(job.readyCount)\n"
        d += "  tuning=\(job.tuning.map { $0.sorted().joined(separator: " ") } ?? "nil")\n"
        d += "  ignored=\(job.ignoredOptions) notes=\(job.targetNotes) clipboard=\(job.clipboardNote ?? "-") paste=\(job.pasteProblem ?? "-")\n"
        for c in job.preset.controls {
            for f in c.flags { d += "  \(f): \(job.reason(f) ?? "on")\n" }
        }
        d += "  command: \(job.commandPreview)\n"
        for f in job.files {
            d += "  \(f.url.lastPathComponent) misfit=\(f.misfit ?? "-") -> \(f.plannedOutput?.lastPathComponent ?? "-") problem=\(f.problem ?? "-") warnings=\(f.warnings) status=\(f.status)\n"
        }
        return d
    }
}

@MainActor
private func pickURLs() -> [URL] {
    let panel = NSOpenPanel()
    panel.allowsMultipleSelection = true
    panel.canChooseDirectories = true
    panel.canChooseFiles = true
    panel.prompt = "Add"
    return panel.runModal() == .OK ? panel.urls : []
}

/// Into a convert-style tool's list.
@MainActor
func pickFiles(_ job: ConvertJob) {
    let urls = pickURLs()
    if !urls.isEmpty { job.add(urls) }
}

/// From Home: into the tool showing if it fits, otherwise Home lists the tools that do.
@MainActor
func pickFiles(_ model: AppModel) {
    let urls = pickURLs()
    if !urls.isEmpty { model.open(urls) }
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
