import SwiftUI

/// The toolbox's sections, in sidebar order.
enum Category: String, CaseIterable, Identifiable {
    case video, audio, images, documents, text, developer, files, conv

    var id: String { rawValue }

    var title: String {
        switch self {
        case .video: "Video"
        case .audio: "Audio"
        case .images: "Images"
        case .documents: "Documents"
        case .text: "Text"
        case .developer: "Developer"
        case .files: "Files"
        case .conv: "conv"
        }
    }

    var color: Color {
        switch self {
        case .video: .purple
        case .audio: .pink
        case .images: .blue
        case .documents: .orange
        case .text: .green
        case .developer: .indigo
        case .files: .teal
        case .conv: .gray
        }
    }

    var symbol: String {
        switch self {
        case .video: "film"
        case .audio: "waveform"
        case .images: "photo"
        case .documents: "doc.text"
        case .text: "textformat"
        case .developer: "curlybraces"
        case .files: "folder"
        case .conv: "terminal"
        }
    }

    /// The conv kind whose files belong here first, for suggestions.
    var kind: String? {
        switch self {
        case .video: "video"
        case .audio: "audio"
        case .images: "image"
        case .documents: "document"
        default: nil
        }
    }
}

/// The knobs a convert-style tool can show.
enum Control: String, CaseIterable {
    case maxSize, resize, quality, colors, fps, crf, time, speed, rotate, strip

    /// conv flags behind the control.
    var flags: [String] {
        switch self {
        case .maxSize: ["--max-size"]
        case .resize: ["--resize"]
        case .quality: ["--quality"]
        case .colors: ["--colors"]
        case .fps: ["--fps"]
        case .crf: ["--crf"]
        case .time: ["--start", "--end", "--duration"]
        case .speed: ["--speed"]
        case .rotate: ["--rotate"]
        case .strip: ["--strip-metadata"]
        }
    }
}

/// What a convert-style tool takes, makes and lets you change. Every one of them runs
/// plain `conv FILE [.ext] [flags]`; the preset only narrows what's offered.
struct ConvertPreset {
    enum Target {
        /// A picker of the targets whose kind is in `kinds` (empty: any target),
        /// led by "Same format" when allowed. `preferred` picks the default.
        case pick(kinds: Set<String>, allowSame: Bool, preferred: [String])
        case fixed(String)
    }

    /// conv kinds the tool takes ("video", "audio", "image", "document").
    var kinds: Set<String> = []
    /// Extensions taken whatever their kind. With no kinds, only these are taken;
    /// with neither, anything is.
    var exts: Set<String> = []
    /// Extensions turned away even when their kind fits.
    var skip: Set<String> = []
    var target: Target
    var controls: [Control]
    /// The tool is this one thing: on, and not shown as a choice.
    var alwaysStrip = false
    /// The main button: "Compress", "Make GIF".
    var action = "Convert"
    /// What a file needs before there's anything to do.
    var needs = "Choose something to change, or pick another format."
    var dropTitle = "Drop files here"
    var dropDetail = "Videos, pictures, audio, documents. Nothing leaves your Mac."
    /// Plural, for "Compress Video takes videos."
    var noun = "files"

    func takes(_ ext: String, kind: String?) -> Bool {
        if skip.contains(ext) { return false }
        if exts.contains(ext) { return true }
        if kinds.isEmpty { return exts.isEmpty }
        return kind.map(kinds.contains) ?? false
    }

    var fixedTarget: String? {
        if case .fixed(let t) = target { return t }
        return nil
    }
}

enum ToolKind {
    case convert(ConvertPreset)
    case trim, mergePDF, photosToPDF, split, scan, qr, ascii, data, backends, formats, changelog
}

/// Every tool in the box. Several are Convert with a narrower form, so each does one
/// job and shows only the knobs for it.
enum ToolID: String, CaseIterable, Identifiable {
    case compressVideo, convertVideo, trimVideo, videoToGIF, extractAudio, videoSpeed, resizeVideo,
         turnVideo, stripVideo
    case convertAudio, trimAudio, audioSpeed, stripAudio
    case convertImage, resizeImage, turnImage, stripImage, makeIcon
    case mergePDF, splitPDF, photosToPDF, convertDocument
    case qr, ascii
    case formatData, convertData
    case convert, scan
    case backends, formats, changelog

    var id: String { rawValue }

    static let popular: [ToolID] = [
        .compressVideo, .convertImage, .videoToGIF, .mergePDF,
        .extractAudio, .trimVideo, .qr, .formatData,
    ]

    static let dataExts: Set<String> = ["json", "yaml", "yml", "toml", "csv"]

    var category: Category {
        switch self {
        case .compressVideo, .convertVideo, .trimVideo, .videoToGIF, .extractAudio, .videoSpeed,
             .resizeVideo, .turnVideo, .stripVideo: .video
        case .convertAudio, .trimAudio, .audioSpeed, .stripAudio: .audio
        case .convertImage, .resizeImage, .turnImage, .stripImage, .makeIcon: .images
        case .mergePDF, .splitPDF, .photosToPDF, .convertDocument: .documents
        case .qr, .ascii: .text
        case .formatData, .convertData: .developer
        case .convert, .scan: .files
        case .backends, .formats, .changelog: .conv
        }
    }

    var title: String {
        switch self {
        case .compressVideo: "Compress Video"
        case .convertVideo: "Convert Video"
        case .trimVideo: "Trim Video"
        case .videoToGIF: "Video to GIF"
        case .extractAudio: "Extract Audio"
        case .videoSpeed, .audioSpeed: "Change Speed"
        case .resizeVideo: "Resize Video"
        case .turnVideo: "Turn Video"
        case .stripVideo, .stripAudio: "Remove Metadata"
        case .convertAudio: "Convert Audio"
        case .trimAudio: "Trim Audio"
        case .convertImage: "Convert Image"
        case .resizeImage: "Resize Image"
        case .turnImage: "Turn Image"
        case .stripImage: "Remove Location"
        case .makeIcon: "Make an Icon"
        case .mergePDF: "Merge PDFs"
        case .splitPDF: "Split PDF"
        case .photosToPDF: "Photos to PDF"
        case .convertDocument: "Convert Document"
        case .qr: "QR Code"
        case .ascii: "ASCII Art"
        case .formatData: "Format JSON"
        case .convertData: "Convert Data"
        case .convert: "Convert Anything"
        case .scan: "Scan Folder"
        case .backends: "Backends"
        case .formats: "Formats"
        case .changelog: "What's New"
        }
    }

    var subtitle: String {
        switch self {
        case .compressVideo: "Make a video small enough to send"
        case .convertVideo: "MOV to MP4, WebM or MKV"
        case .trimVideo: "Cut clips out of a video"
        case .videoToGIF: "A looping GIF from part of a video"
        case .extractAudio: "Save a video's sound as MP3, M4A or WAV"
        case .videoSpeed: "Speed up or slow down a video or GIF"
        case .resizeVideo: "Make a video's picture smaller"
        case .turnVideo: "Fix a sideways video"
        case .stripVideo: "Take out where and when it was filmed"
        case .convertAudio: "MP3, M4A, WAV, FLAC, Opus and Ogg"
        case .trimAudio: "Cut clips out of a recording"
        case .audioSpeed: "Faster or slower, with or without the pitch"
        case .stripAudio: "Keep title, artist and album, drop the rest"
        case .convertImage: "HEIC to JPG, PNG, WebP and more"
        case .resizeImage: "Make a picture smaller"
        case .turnImage: "Fix a sideways photo"
        case .stripImage: "Take location and camera details out of photos"
        case .makeIcon: "An ICO with every size, from a picture or SVG"
        case .mergePDF: "Join PDFs into one"
        case .splitPDF: "A file a page, or a file for each range"
        case .photosToPDF: "Put pictures into one PDF, a page each"
        case .convertDocument: "Word, Markdown, Office files and PDF"
        case .qr: "A code for a link or any text"
        case .ascii: "Turn a picture into text"
        case .formatData: "Tidy, pack or check JSON, YAML and TOML"
        case .convertData: "JSON, YAML, TOML and CSV, any way round"
        case .convert: "Every format and every option in one place"
        case .scan: "See what a folder's files could become"
        case .backends: "The programs conv uses, and installing them"
        case .formats: "Every format and what it turns into"
        case .changelog: "What each version of conv added"
        }
    }

    var symbol: String {
        switch self {
        case .compressVideo: "arrow.down.right.and.arrow.up.left"
        case .convertVideo, .convertAudio, .convertImage, .convert: "arrow.triangle.2.circlepath"
        case .trimVideo, .trimAudio: "scissors"
        case .videoToGIF: "play.rectangle.on.rectangle"
        case .extractAudio: "music.note"
        case .videoSpeed, .audioSpeed: "hare"
        case .resizeVideo, .resizeImage: "arrow.up.left.and.arrow.down.right"
        case .turnVideo, .turnImage: "rotate.right"
        case .stripVideo, .stripImage: "location.slash"
        case .stripAudio: "tag.slash"
        case .makeIcon: "app.dashed"
        case .mergePDF: "doc.on.doc"
        case .splitPDF: "doc.badge.ellipsis"
        case .photosToPDF: "photo.on.rectangle.angled"
        case .convertDocument: "doc.richtext"
        case .qr: "qrcode"
        case .ascii: "text.below.photo"
        case .formatData: "curlybraces"
        case .convertData: "arrow.left.arrow.right"
        case .scan: "folder.badge.questionmark"
        case .backends: "shippingbox"
        case .formats: "list.bullet.rectangle"
        case .changelog: "sparkles"
        }
    }

    /// Words people search with that aren't in the title.
    var keywords: String {
        switch self {
        case .compressVideo: "shrink smaller reduce size mb email discord whatsapp send mp4 mov"
        case .convertVideo: "mov mp4 webm mkv avi format change export"
        case .trimVideo, .trimAudio: "cut clip shorten segment start end"
        case .videoToGIF: "gif animated animation loop meme"
        case .extractAudio: "sound mp3 m4a wav soundtrack rip audio from video"
        case .videoSpeed, .audioSpeed: "fast slow motion timelapse speed up slow down 2x faster slower"
        case .resizeVideo: "scale dimensions 1080p 720p resolution width smaller"
        case .turnVideo, .turnImage: "rotate sideways orientation portrait landscape 90 upside down"
        case .stripVideo, .stripImage: "privacy gps location exif metadata strip date camera"
        case .stripAudio: "tags metadata strip privacy"
        case .convertAudio: "mp3 m4a wav flac opus ogg format music"
        case .convertImage: "heic jpg jpeg png webp avif tiff bmp svg photo picture iphone format"
        case .resizeImage: "scale dimensions smaller width photo picture shrink"
        case .makeIcon: "ico favicon windows icon svg"
        case .mergePDF: "combine join append pdf"
        case .splitPDF: "pages extract separate pdf"
        case .photosToPDF: "images jpg png pictures scans pdf combine"
        case .convertDocument: "word docx doc pdf markdown md html office excel xlsx powerpoint pptx odt spreadsheet"
        case .qr: "qr link url barcode wifi"
        case .ascii: "text art characters braille"
        case .formatData: "json yaml toml pretty print beautify minify validate lint sort keys check"
        case .convertData: "json yaml toml csv xlsx spreadsheet data"
        case .convert: "format batch everything"
        case .scan: "folder batch"
        case .backends: "ffmpeg imagemagick pandoc typst libreoffice qpdf install update doctor"
        case .formats: "capabilities supported list"
        case .changelog: "changelog version release notes"
        }
    }

    /// The conv subcommand this tool needs, if any.
    var subcommand: String? {
        switch self {
        case .trimVideo, .trimAudio: "trim"
        case .mergePDF: "merge"
        case .splitPDF: "split"
        case .scan: "scan"
        case .qr: "qr"
        case .ascii: "ascii"
        case .formatData: "fmt"
        case .backends: "doctor"
        case .formats: "capabilities"
        case .changelog: "changelog"
        default: nil
        }
    }

    var kind: ToolKind {
        switch self {
        case .compressVideo:
            .convert(ConvertPreset(
                kinds: ["video"], target: .pick(kinds: ["video"], allowSame: true, preferred: []),
                controls: [.maxSize], action: "Compress", needs: "Choose a size to stay under.",
                dropTitle: "Drop videos to compress",
                dropDetail: "Pick a size, and conv chooses the resolution and bitrate that fit it.",
                noun: "videos"))
        case .convertVideo:
            .convert(ConvertPreset(
                kinds: ["video"], target: .pick(kinds: ["video"], allowSame: false, preferred: ["mp4", "mov", "webm"]),
                controls: [.resize, .crf, .fps], dropTitle: "Drop videos to convert",
                dropDetail: "MOV, MP4, MKV, WebM and AVI.", noun: "videos"))
        case .videoToGIF:
            .convert(ConvertPreset(
                kinds: ["video"], target: .fixed("gif"), controls: [.time, .resize, .fps, .speed],
                action: "Make GIF", dropTitle: "Drop a video to make a GIF",
                dropDetail: "Keep just the part you want. A smaller size and frame rate make a smaller GIF.",
                noun: "videos"))
        case .extractAudio:
            .convert(ConvertPreset(
                kinds: ["video"], target: .pick(kinds: ["audio"], allowSame: false, preferred: ["mp3", "m4a"]),
                controls: [.time], action: "Extract", dropTitle: "Drop videos to take the sound from",
                dropDetail: "The picture is left out.", noun: "videos"))
        case .videoSpeed:
            .convert(ConvertPreset(
                kinds: ["video"], exts: ["gif"], target: .pick(kinds: ["video"], allowSame: true, preferred: []),
                controls: [.speed], action: "Change Speed", needs: "Choose a speed.",
                dropTitle: "Drop videos or GIFs", dropDetail: "The sound keeps its pitch unless you say otherwise.",
                noun: "videos and GIFs"))
        case .resizeVideo:
            .convert(ConvertPreset(
                kinds: ["video"], exts: ["gif"], target: .pick(kinds: ["video"], allowSame: true, preferred: []),
                controls: [.resize], action: "Resize", needs: "Choose a size.",
                dropTitle: "Drop videos to resize", dropDetail: "A video already smaller is left alone.",
                noun: "videos and GIFs"))
        case .turnVideo:
            .convert(ConvertPreset(
                kinds: ["video"], exts: ["gif"], target: .pick(kinds: ["video"], allowSame: true, preferred: []),
                controls: [.rotate], action: "Turn", needs: "Choose which way to turn it.",
                dropTitle: "Drop sideways videos",
                dropDetail: "One that's sideways only because of its orientation tag is fixed without re-encoding.",
                noun: "videos and GIFs"))
        case .stripVideo:
            .convert(ConvertPreset(
                kinds: ["video"], target: .pick(kinds: ["video"], allowSame: true, preferred: []),
                controls: [], alwaysStrip: true, action: "Remove Metadata", needs: "conv can't take the metadata out of this file.",
                dropTitle: "Drop videos to clean",
                dropDetail: "Location, dates and the rest go. Title, artist, album and track numbers stay.",
                noun: "videos"))
        case .convertAudio:
            .convert(ConvertPreset(
                kinds: ["audio"], target: .pick(kinds: ["audio"], allowSame: false, preferred: ["mp3", "m4a"]),
                controls: [], dropTitle: "Drop audio files to convert",
                dropDetail: "MP3, M4A, WAV, FLAC, Opus and Ogg.", noun: "audio files"))
        case .audioSpeed:
            .convert(ConvertPreset(
                kinds: ["audio"], target: .pick(kinds: ["audio"], allowSame: true, preferred: []),
                controls: [.speed], action: "Change Speed", needs: "Choose a speed.",
                dropTitle: "Drop audio files", dropDetail: "The sound keeps its pitch unless you say otherwise.",
                noun: "audio files"))
        case .stripAudio:
            .convert(ConvertPreset(
                kinds: ["audio"], target: .pick(kinds: ["audio"], allowSame: true, preferred: []),
                controls: [], alwaysStrip: true, action: "Remove Metadata", needs: "conv can't take the metadata out of this file.", dropTitle: "Drop audio files to clean",
                dropDetail: "Title, artist, album, composer, genre and track numbers stay. Everything else goes.",
                noun: "audio files"))
        case .convertImage:
            .convert(ConvertPreset(
                kinds: ["image"], target: .pick(kinds: ["image"], allowSame: false, preferred: ["jpg", "png", "webp"]),
                controls: [.quality, .resize, .colors], dropTitle: "Drop pictures to convert",
                dropDetail: "HEIC, JPG, PNG, WebP, AVIF, TIFF, BMP and SVG.", noun: "pictures"))
        case .resizeImage:
            .convert(ConvertPreset(
                kinds: ["image"], target: .pick(kinds: ["image"], allowSame: true, preferred: []),
                controls: [.resize, .quality], action: "Resize", needs: "Choose a size.",
                dropTitle: "Drop pictures to resize", dropDetail: "A picture already smaller is left alone.",
                noun: "pictures"))
        case .turnImage:
            .convert(ConvertPreset(
                kinds: ["image"], target: .pick(kinds: ["image"], allowSame: true, preferred: []),
                controls: [.rotate], action: "Turn", needs: "Choose which way to turn it.",
                dropTitle: "Drop sideways photos", dropDetail: "The picture itself is turned, so every app shows it the same way.",
                noun: "pictures"))
        case .stripImage:
            .convert(ConvertPreset(
                kinds: ["image"], target: .pick(kinds: ["image"], allowSame: true, preferred: []),
                controls: [], alwaysStrip: true, action: "Remove", needs: "conv can't take the metadata out of this file.", dropTitle: "Drop photos to clean",
                dropDetail: "Location, camera and dates go. The colour profile stays, and the photo is turned the way it's shown first.",
                noun: "pictures"))
        case .makeIcon:
            .convert(ConvertPreset(
                kinds: ["image"], target: .fixed("ico"), controls: [], action: "Make Icon",
                dropTitle: "Drop a picture or SVG",
                dropDetail: "A square picture works best. Every size from 16 to 256 goes into the one file.",
                noun: "pictures"))
        case .convertDocument:
            .convert(ConvertPreset(
                kinds: ["document"], skip: Self.dataExts,
                target: .pick(kinds: ["document"], allowSame: false, preferred: ["pdf", "docx"]),
                controls: [], dropTitle: "Drop documents to convert",
                dropDetail: "Word, Markdown, Excel, PowerPoint, OpenDocument and PDF.", noun: "documents"))
        case .convertData:
            .convert(ConvertPreset(
                exts: Self.dataExts, target: .pick(kinds: [], allowSame: false, preferred: ["json", "yaml"]),
                controls: [], dropTitle: "Drop JSON, YAML, TOML or CSV files",
                dropDetail: "Every key stays in its place and every number keeps its digits.", noun: "data files"))
        case .convert:
            .convert(ConvertPreset(
                target: .pick(kinds: [], allowSame: true, preferred: []),
                controls: Control.allCases))
        case .trimVideo, .trimAudio: .trim
        case .mergePDF: .mergePDF
        case .photosToPDF: .photosToPDF
        case .splitPDF: .split
        case .scan: .scan
        case .qr: .qr
        case .ascii: .ascii
        case .formatData: .data
        case .backends: .backends
        case .formats: .formats
        case .changelog: .changelog
        }
    }

    var preset: ConvertPreset? {
        if case .convert(let p) = kind { return p }
        return nil
    }

    /// Text a search is matched against.
    var searchText: String {
        [title, subtitle, keywords, category.title].joined(separator: " ").lowercased()
    }
}

/// A tool's glyph on a tile of its section's colour, like System Settings.
struct ToolIcon: View {
    let tool: ToolID
    var size: CGFloat = 20

    var body: some View {
        TileIcon(symbol: tool.symbol, color: tool.category.color, size: size)
    }
}

struct TileIcon: View {
    let symbol: String
    let color: Color
    var size: CGFloat = 20

    var body: some View {
        RoundedRectangle(cornerRadius: size * 0.26, style: .continuous)
            .fill(color.gradient)
            .frame(width: size, height: size)
            .overlay {
                Image(systemName: symbol)
                    .font(.system(size: size * 0.52, weight: .semibold))
                    .foregroundStyle(.white)
            }
            .accessibilityHidden(true)
    }
}
