import AppKit
import Foundation

/// What Paste brings in. Files copied in Finder come as themselves. A copied picture
/// or screenshot is saved once by `conv --paste` into a folder of the app's own, so
/// copying something else later doesn't change what gets converted.
enum Clipboard {
    struct Problem: Error { var message: String }

    /// Pictures saved from the clipboard, so their results can go somewhere sensible.
    nonisolated(unsafe) static var pastedPictures: Set<URL> = []

    static func isPasted(_ url: URL) -> Bool { pastedPictures.contains(url.standardizedFileURL) }

    /// Where results of a pasted picture go when no folder is chosen.
    static var pastedOutputFolder: URL {
        FileManager.default.urls(for: .downloadsDirectory, in: .userDomainMask).first
            ?? FileManager.default.homeDirectoryForCurrentUser
    }

    /// On the main actor: it reads the pasteboard and writes `pastedPictures`.
    @MainActor
    static func take(conv: URL) async -> Result<[URL], Problem> {
        let files = (NSPasteboard.general.readObjects(
            forClasses: [NSURL.self], options: [.urlReadingFileURLsOnly: true]) as? [URL]) ?? []
        if !files.isEmpty { return .success(files) }

        // Kept until the next launch, since a tool may still be using the picture.
        let folder = appTemp.appendingPathComponent("convkit-paste-\(UUID().uuidString)", isDirectory: true)
        do {
            try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        } catch {
            return .failure(Problem(message: error.localizedDescription))
        }
        // `conv --paste` alone saves a copied picture as it is, as clipboard.EXT here.
        guard let out = try? await Conv.run(conv, ["--paste", "--json", "--no-install"], in: folder) else {
            try? FileManager.default.removeItem(at: folder)
            return .failure(Problem(message: "conv didn't answer."))
        }
        let resp = out.decode(RunResponse.self)
        if let r = resp?.results?.first, r.ok, let path = r.output {
            let url = URL(fileURLWithPath: path, relativeTo: folder).standardizedFileURL
            pastedPictures.insert(url)
            return .success([url])
        }
        try? FileManager.default.removeItem(at: folder)
        let err = resp?.results?.first?.error ?? resp?.error
        return .failure(Problem(message: err.map(AppModel.friendly) ?? Conv.stderrMessage(out.stderr)))
    }

    /// Several outputs go on the clipboard as files, as `conv --copy` does.
    @MainActor
    static func copyFiles(_ urls: [URL]) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.writeObjects(urls as [NSURL])
    }
}
