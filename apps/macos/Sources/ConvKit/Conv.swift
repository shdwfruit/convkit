import Foundation

/// Talks to the `conv` binary. The app never re-implements conv's rules:
/// targets come from `conv capabilities`, names and problems from
/// `--dry-run --json`, and results from `--json`.
enum Conv {
    /// Where to look for conv, in order. A path set in Settings wins.
    static func locate(override: String) -> URL? {
        let fm = FileManager.default
        let home = fm.homeDirectoryForCurrentUser.path
        var candidates: [String] = []
        if !override.isEmpty { candidates.append((override as NSString).expandingTildeInPath) }
        if let env = ProcessInfo.processInfo.environment["CONV_BIN"] { candidates.append(env) }
        if let bundled = Bundle.main.resourceURL?.appendingPathComponent("conv").path {
            candidates.append(bundled)
        }
        candidates += [
            "\(home)/.local/bin/conv",
            "/opt/homebrew/bin/conv",
            "/usr/local/bin/conv",
            "\(home)/.cargo/bin/conv",
        ]
        return candidates.first { fm.isExecutableFile(atPath: $0) }.map(URL.init(fileURLWithPath:))
    }

    struct Output {
        var stdout: Data
        var stderr: String
        var status: Int32

        /// conv prints results on stdout and a whole-run error as JSON on stderr.
        func decode<T: Decodable>(_ type: T.Type) -> T? {
            (try? JSONDecoder().decode(T.self, from: stdout))
                ?? (try? JSONDecoder().decode(T.self, from: Data(stderr.utf8)))
        }
    }

    /// Runs conv with `args`. Cancelling the calling task stops the process.
    static func run(_ binary: URL, _ args: [String], in folder: URL? = nil) async throws -> Output {
        let process = Process()
        process.executableURL = binary
        process.arguments = args
        if let folder { process.currentDirectoryURL = folder }
        process.standardInput = FileHandle.nullDevice
        // A Finder-launched app gets a minimal PATH; let conv find Homebrew backends.
        var env = ProcessInfo.processInfo.environment
        env["PATH"] = "/opt/homebrew/bin:/usr/local/bin:" + (env["PATH"] ?? "/usr/bin:/bin")
        process.environment = env

        let out = Pipe()
        let err = Pipe()
        process.standardOutput = out
        process.standardError = err

        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Output, Error>) in
                // Drain both pipes concurrently so a chatty backend can't fill one and stall.
                let group = DispatchGroup()
                let collected = Collected()
                group.enter()
                DispatchQueue.global().async {
                    collected.out = out.fileHandleForReading.readDataToEndOfFile()
                    group.leave()
                }
                group.enter()
                DispatchQueue.global().async {
                    collected.err = err.fileHandleForReading.readDataToEndOfFile()
                    group.leave()
                }
                process.terminationHandler = { p in
                    group.notify(queue: .global()) {
                        cont.resume(returning: Output(
                            stdout: collected.out,
                            stderr: String(decoding: collected.err, as: UTF8.self),
                            status: p.terminationStatus
                        ))
                    }
                }
                do {
                    try process.run()
                } catch {
                    process.terminationHandler = nil
                    cont.resume(throwing: error)
                }
            }
        } onCancel: {
            if process.isRunning { process.terminate() }
        }
    }

    /// Each pipe is written by one reader, and read only after the group finishes.
    private final class Collected: @unchecked Sendable {
        var out = Data()
        var err = Data()
    }

    /// A readable line from stderr for when conv didn't print JSON (a usage error).
    static func stderrMessage(_ stderr: String) -> String {
        let line = stderr
            .split(separator: "\n")
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .first { !$0.isEmpty } ?? "conv failed"
        return line.hasPrefix("error: ") ? String(line.dropFirst(7)) : line
    }
}

// MARK: - JSON shapes (only the fields the app reads)

struct ConvError: Decodable, Hashable {
    var code: String
    var message: String
}

struct CapabilitiesResponse: Decodable {
    struct Target: Decodable {
        var to: String
        var tuning: [String]
        var notes: [String]?
    }
    var ok: Bool
    var format: String?
    var kind: String?
    var targets: [Target]?
    var error: ConvError?
}

struct DryRunResponse: Decodable {
    struct Entry: Decodable {
        var ok: Bool
        var plan: Plan?
        var error: ConvError?
    }
    struct Plan: Decodable {
        var output: String?
        var warnings: [String]?
        var sizing: Sizing?
    }
    struct Sizing: Decodable {
        var warning: String?
        var suggested: String?
    }
    var ok: Bool
    var plans: [Entry]?
    var error: ConvError?
}

struct RunResponse: Decodable {
    /// `--copy`'s report: what went on the clipboard, or why it couldn't.
    struct ClipboardReport: Decodable {
        var ok: Bool
        var message: String
    }
    var clipboard: ClipboardReport?
    struct Result: Decodable {
        var ok: Bool
        var output: String?
        var bytes: Int64?
        var warnings: [String]?
        var notes: [String]?
        var error: ConvError?
    }
    var ok: Bool
    var results: [Result]?
    var error: ConvError?
}
