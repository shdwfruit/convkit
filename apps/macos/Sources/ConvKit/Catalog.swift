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
        case .files: "archive"
        default: nil
        }
    }
}

/// The knobs a convert-style tool can show.
enum Control: String, CaseIterable {
    case maxSize, compress, resize, quality, colors, fps, crf, crop, rotate, flip, time, every, speed,
         denoise, audio, subtitles, pages, dpi, lock, unlock, strip

    /// conv flags behind the control.
    var flags: [String] {
        switch self {
        case .maxSize: ["--max-size"]
        case .compress: ["--compress"]
        case .resize: ["--resize"]
        case .quality: ["--quality"]
        case .colors: ["--colors"]
        case .fps: ["--fps"]
        case .crf: ["--crf"]
        case .crop: ["--crop"]
        case .rotate: ["--rotate"]
        case .flip: ["--flip"]
        case .time: ["--start", "--end", "--duration"]
        case .every: ["--every"]
        case .speed: ["--speed"]
        case .denoise: ["--denoise"]
        case .audio: ["--audio"]
        case .subtitles: ["--subtitles"]
        case .pages: ["--pages"]
        case .dpi: ["--dpi"]
        case .lock: ["--password"]
        case .unlock: ["--unlock"]
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
        /// The file stays in its own format (Compress PDF, Lock PDF).
        case keep
    }

    /// conv kinds the tool takes ("video", "audio", "image", "document").
    var kinds: Set<String> = []
    /// Extensions taken whatever their kind. With no kinds, only these are taken;
    /// with neither, anything is.
    var exts: Set<String> = []
    /// Extensions turned away even when their kind fits.
    var skip: Set<String> = []
    var target: Target
    /// Targets not offered though their kind fits (a GIF isn't a still).
    var skipTargets: Set<String> = []
    var controls: [Control]
    /// Controls that start switched on, because they are the tool (Remove Noise).
    var startsOn: Set<Control> = []
    /// The tool is this one thing: on, and not shown as a choice.
    var alwaysStrip = false
    var alwaysUnlock = false
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

    /// Flags that are the tool itself, so a file counts as fitting when one applies.
    var ownFlags: Set<String> {
        var flags = Set(controls.flatMap(\.flags))
        if alwaysStrip { flags.insert("--strip-metadata") }
        if alwaysUnlock { flags.insert("--unlock") }
        return flags
    }
}

enum ToolKind {
    case convert(ConvertPreset)
    case trim, merge, split, pages, scan, qr, ascii, data, quick, speak, wifi, beautify, colors,
         makeArchive, openArchive, backends, formats, changelog
}

/// Every tool in the box. Several are Convert with a narrower form, so each does one
/// job and shows only the knobs for it.
enum ToolID: String, CaseIterable, Identifiable {
    case compressVideo, convertVideo, trimVideo, joinVideos, videoToGIF, videoStills, extractAudio,
         addAudio, burnSubtitles, cropVideo, turnVideo, resizeVideo, videoSpeed, stripVideo
    case convertAudio, trimAudio, joinAudio, denoise, audioSpeed, stripAudio
    case convertImage, resizeImage, cropImage, turnImage, beautify, colors, makeIcon, stripImage
    case compressPDF, mergePDF, splitPDF, pdfPages, pdfToImages, photosToPDF, lockPDF, unlockPDF,
         convertDocument
    case qr, readQR, speak, ascii, count, changeCase, cleanLinks, findChars, compareText, lorem, wifi
    case formatData, convertData, base64, hash, jwt, timestamp, uuid
    case convert, info, makeArchive, openArchive, scan
    case backends, formats, changelog

    var id: String { rawValue }

    static let popular: [ToolID] = [
        .compressVideo, .convertImage, .compressPDF, .mergePDF,
        .videoToGIF, .trimVideo, .extractAudio, .qr,
    ]

    static let dataExts: Set<String> = ["json", "yaml", "yml", "toml", "csv"]
    /// What `conv speak -f` reads aloud.
    static let speakExts: Set<String> = ["txt", "md", "html", "docx", "odt", "rtf"]

    var category: Category {
        switch self {
        case .compressVideo, .convertVideo, .trimVideo, .joinVideos, .videoToGIF, .videoStills,
             .extractAudio, .addAudio, .burnSubtitles, .cropVideo, .turnVideo, .resizeVideo,
             .videoSpeed, .stripVideo: .video
        case .convertAudio, .trimAudio, .joinAudio, .denoise, .audioSpeed, .stripAudio: .audio
        case .convertImage, .resizeImage, .cropImage, .turnImage, .beautify, .colors, .makeIcon,
             .stripImage: .images
        case .compressPDF, .mergePDF, .splitPDF, .pdfPages, .pdfToImages, .photosToPDF, .lockPDF,
             .unlockPDF, .convertDocument: .documents
        case .qr, .readQR, .speak, .ascii, .count, .changeCase, .cleanLinks, .findChars, .compareText,
             .lorem, .wifi: .text
        case .formatData, .convertData, .base64, .hash, .jwt, .timestamp, .uuid: .developer
        case .convert, .info, .makeArchive, .openArchive, .scan: .files
        case .backends, .formats, .changelog: .conv
        }
    }

    var title: String {
        switch self {
        case .compressVideo: "Compress Video"
        case .convertVideo: "Convert Video"
        case .trimVideo: "Trim Video"
        case .joinVideos: "Join Videos"
        case .videoToGIF: "Video to GIF"
        case .videoStills: "Video to Images"
        case .extractAudio: "Extract Audio"
        case .addAudio: "Add Audio"
        case .burnSubtitles: "Burn Subtitles"
        case .cropVideo: "Crop Video"
        case .turnVideo: "Turn or Flip Video"
        case .resizeVideo: "Resize Video"
        case .videoSpeed, .audioSpeed: "Change Speed"
        case .stripVideo, .stripAudio: "Remove Metadata"
        case .convertAudio: "Convert Audio"
        case .trimAudio: "Trim Audio"
        case .joinAudio: "Join Audio"
        case .denoise: "Remove Noise"
        case .convertImage: "Convert Image"
        case .resizeImage: "Resize Image"
        case .cropImage: "Crop Image"
        case .turnImage: "Turn or Flip Image"
        case .beautify: "Beautify Screenshot"
        case .colors: "Color Palette"
        case .makeIcon: "Make an Icon"
        case .stripImage: "Remove Location"
        case .compressPDF: "Compress PDF"
        case .mergePDF: "Merge PDFs"
        case .splitPDF: "Split PDF"
        case .pdfPages: "Arrange Pages"
        case .pdfToImages: "PDF to Images"
        case .photosToPDF: "Photos to PDF"
        case .lockPDF: "Lock PDF"
        case .unlockPDF: "Unlock PDF"
        case .convertDocument: "Convert Document"
        case .qr: "QR Code"
        case .readQR: "Read QR Code"
        case .speak: "Text to Speech"
        case .ascii: "ASCII Art"
        case .count: "Word Count"
        case .changeCase: "Change Case"
        case .cleanLinks: "Clean Links"
        case .findChars: "Hidden Characters"
        case .compareText: "Compare Text"
        case .lorem: "Lorem Ipsum"
        case .wifi: "Wi-Fi Password"
        case .formatData: "Format JSON"
        case .convertData: "Convert Data"
        case .base64: "Base64"
        case .hash: "Checksum"
        case .jwt: "JWT Decoder"
        case .timestamp: "Timestamps"
        case .uuid: "UUID"
        case .convert: "Convert Anything"
        case .info: "File Info"
        case .makeArchive: "Zip Files"
        case .openArchive: "Open Archive"
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
        case .joinVideos: "Put clips one after another in one video"
        case .videoToGIF: "A looping GIF from part of a video"
        case .videoStills: "Save stills from a video or GIF"
        case .extractAudio: "Save a video's sound as MP3, M4A or WAV"
        case .addAudio: "Put music or a voice-over on a video"
        case .burnSubtitles: "Put an SRT, VTT or ASS file's subtitles on the picture"
        case .cropVideo: "9:16 for Stories, 1:1, or cut away black bars"
        case .turnVideo: "Fix a sideways or mirrored video"
        case .resizeVideo: "Make a video's picture smaller"
        case .videoSpeed: "Speed up or slow down a video or GIF"
        case .stripVideo: "Take out where and when it was filmed"
        case .convertAudio: "MP3, M4A, WAV, FLAC, AIFF, Opus and Ogg"
        case .trimAudio: "Cut clips out of a recording"
        case .joinAudio: "Put recordings one after another in one file"
        case .denoise: "Take background noise and hum out of a recording"
        case .audioSpeed: "Faster or slower, with or without the pitch"
        case .stripAudio: "Keep title, artist and album, drop the rest"
        case .convertImage: "HEIC to JPG, PNG, WebP and more"
        case .resizeImage: "Make a picture smaller"
        case .cropImage: "To a shape like 1:1 or 16:9, or trim a plain border"
        case .turnImage: "Fix a sideways or mirrored photo"
        case .beautify: "Set a screenshot on a background, with a shadow"
        case .colors: "A picture's main colours, as HEX, RGB and code"
        case .makeIcon: "An ICO with every size, from a picture or SVG"
        case .stripImage: "Take location and camera details out of photos"
        case .compressPDF: "Make a PDF smaller with the same look"
        case .mergePDF: "Join PDFs, and pictures among them, into one"
        case .splitPDF: "A file a page, or a file for each range"
        case .pdfPages: "Reorder, delete or turn a PDF's pages"
        case .pdfToImages: "Each page as a PNG or JPG"
        case .photosToPDF: "Put pictures into one PDF, a page each"
        case .lockPDF: "Add a password, and turn off printing or copying"
        case .unlockPDF: "Take the password off a PDF you can open"
        case .convertDocument: "Word, Markdown, text, Office files and PDF"
        case .qr: "A code for a link or any text"
        case .readQR: "What a code in a picture or screenshot holds"
        case .speak: "Read text aloud into a sound file"
        case .ascii: "Turn a picture into text"
        case .count: "Words, characters, sentences and reading time"
        case .changeCase: "UPPER, lower, Title, camelCase, snake_case and more"
        case .cleanLinks: "Take the tracking out of links"
        case .findChars: "Find invisible and lookalike characters"
        case .compareText: "What changed between two texts"
        case .lorem: "Placeholder text"
        case .wifi: "A saved network's password, or a code that joins it"
        case .formatData: "Tidy, pack or check JSON, YAML and TOML"
        case .convertData: "JSON, YAML, TOML and CSV, any way round"
        case .base64: "Encode and decode base64 and data: URIs"
        case .hash: "SHA-256 and MD5, and checking a download"
        case .jwt: "A token's claims and expiry, and its signature"
        case .timestamp: "Unix time to a date and back"
        case .uuid: "Make UUIDs, or see what one is"
        case .convert: "Every format and every option in one place"
        case .info: "What a file is: format, size, streams, pages"
        case .makeArchive: "Pack files and folders into a zip or tar"
        case .openArchive: "Unpack a zip, tar.gz or 7z"
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
        case .joinVideos: "film.stack"
        case .videoToGIF: "play.rectangle.on.rectangle"
        case .videoStills: "photo.stack"
        case .extractAudio: "music.note"
        case .addAudio: "speaker.wave.2"
        case .burnSubtitles: "captions.bubble"
        case .cropVideo, .cropImage: "crop"
        case .videoSpeed, .audioSpeed: "hare"
        case .resizeVideo, .resizeImage: "arrow.up.left.and.arrow.down.right"
        case .turnVideo, .turnImage: "rotate.right"
        case .stripVideo, .stripImage: "location.slash"
        case .stripAudio: "tag.slash"
        case .joinAudio: "waveform.badge.plus"
        case .denoise: "waveform.badge.minus"
        case .beautify: "sparkles.rectangle.stack"
        case .colors: "paintpalette"
        case .makeIcon: "app.dashed"
        case .compressPDF: "arrow.down.doc"
        case .mergePDF: "doc.on.doc"
        case .splitPDF: "doc.badge.ellipsis"
        case .pdfPages: "rectangle.stack"
        case .pdfToImages: "photo.on.rectangle"
        case .photosToPDF: "photo.on.rectangle.angled"
        case .lockPDF: "lock.doc"
        case .unlockPDF: "lock.open"
        case .convertDocument: "doc.richtext"
        case .qr: "qrcode"
        case .readQR: "qrcode.viewfinder"
        case .speak: "person.wave.2"
        case .ascii: "text.below.photo"
        case .count: "number"
        case .changeCase: "textformat.size"
        case .cleanLinks: "link"
        case .findChars: "eye.trianglebadge.exclamationmark"
        case .compareText: "plusminus"
        case .lorem: "text.alignleft"
        case .wifi: "wifi"
        case .formatData: "curlybraces"
        case .convertData: "arrow.left.arrow.right"
        case .base64: "textformat.alt"
        case .hash: "checkmark.seal"
        case .jwt: "key"
        case .timestamp: "clock"
        case .uuid: "dice"
        case .info: "info.circle"
        case .makeArchive: "archivebox"
        case .openArchive: "tray.and.arrow.up"
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
        case .joinVideos: "merge combine concatenate append clips stitch"
        case .videoToGIF: "gif animated animation loop meme"
        case .videoStills: "frames stills screenshot thumbnail extract frame png jpg every"
        case .extractAudio: "sound mp3 m4a wav soundtrack rip audio from video"
        case .addAudio: "music song soundtrack voiceover voice-over replace sound mix background"
        case .burnSubtitles: "captions srt vtt ass hardcode subs"
        case .cropVideo: "aspect ratio vertical square tiktok reels shorts instagram black bars letterbox"
        case .cropImage: "aspect ratio square border trim cut"
        case .videoSpeed, .audioSpeed: "fast slow motion timelapse speed up slow down 2x faster slower"
        case .resizeVideo: "scale dimensions 1080p 720p resolution width smaller"
        case .turnVideo, .turnImage: "rotate sideways orientation portrait landscape 90 upside down flip mirror"
        case .stripVideo, .stripImage: "privacy gps location exif metadata strip date camera"
        case .stripAudio: "tags metadata strip privacy"
        case .convertAudio: "mp3 m4a wav flac aiff opus ogg format music"
        case .joinAudio: "merge combine concatenate album tracks"
        case .denoise: "denoise hiss hum clean up podcast voice background noise reduction"
        case .convertImage: "heic jpg jpeg png webp avif tiff bmp svg photo picture iphone format"
        case .resizeImage: "scale dimensions smaller width photo picture shrink"
        case .beautify: "screenshot background gradient shadow padding rounded corners mockup social post"
        case .colors: "colours palette hex rgb hsl eyedropper picker extract swiftui css tailwind"
        case .makeIcon: "ico favicon windows icon svg"
        case .compressPDF: "shrink smaller reduce size pdf email"
        case .mergePDF: "combine join append pdf pictures"
        case .splitPDF: "pages extract separate pdf"
        case .pdfPages: "reorder delete remove rotate pages pdf organize extract"
        case .pdfToImages: "pdf png jpg pages pictures export dpi"
        case .photosToPDF: "images jpg png pictures scans pdf combine"
        case .lockPDF: "password protect encrypt secure restrict"
        case .unlockPDF: "password remove decrypt unprotect restrictions"
        case .convertDocument: "word docx doc pdf markdown md html txt rtf office excel xlsx powerpoint pptx odt spreadsheet"
        case .qr: "qr link url barcode wifi"
        case .readQR: "scan decode qr wifi barcode"
        case .speak: "tts read aloud speak voice say audio narration"
        case .ascii: "text art characters braille"
        case .count: "count characters words letters reading time"
        case .changeCase: "uppercase lowercase title case camel snake kebab pascal constant"
        case .cleanLinks: "url utm tracking fbclid redirect unwrap safe links"
        case .findChars: "unicode zero width invisible homoglyph lookalike bidi characters inspect"
        case .compareText: "diff compare changes difference"
        case .lorem: "placeholder dummy filler text lorem ipsum"
        case .wifi: "wifi password network share qr join"
        case .formatData: "json yaml toml pretty print beautify minify validate lint sort keys check"
        case .convertData: "json yaml toml csv xlsx spreadsheet data"
        case .base64: "encode decode data uri base64url"
        case .hash: "hash sha256 sha1 md5 sha512 verify download integrity sums"
        case .jwt: "json web token jwt bearer claims decode verify hmac"
        case .timestamp: "unix epoch time date timestamp timezone convert iso"
        case .uuid: "uuid guid random id v4 v7"
        case .convert: "format batch everything"
        case .info: "metadata details codec resolution duration bitrate exif probe properties"
        case .makeArchive: "zip compress archive tar gz pack password"
        case .openArchive: "unzip extract unpack zip tar 7z gz decompress"
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
        case .mergePDF, .joinVideos, .joinAudio: "merge"
        case .splitPDF: "split"
        case .pdfPages: "pages"
        case .scan: "scan"
        case .qr, .readQR: "qr"
        case .speak: "speak"
        case .ascii: "ascii"
        case .beautify: "beautify"
        case .colors: "colors"
        case .wifi: "wifi"
        case .info: "info"
        case .formatData: "fmt"
        case .base64: "base64"
        case .hash: "hash"
        case .jwt: "jwt"
        case .timestamp: "time"
        case .uuid: "uuid"
        case .count: "count"
        case .changeCase: "case"
        case .cleanLinks: "link"
        case .findChars: "chars"
        case .compareText: "diff"
        case .lorem: "lorem"
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
                kinds: ["video"], target: .fixed("gif"), controls: [.time, .resize, .fps, .speed, .crop],
                action: "Make GIF", dropTitle: "Drop a video to make a GIF",
                dropDetail: "Keep just the part you want. A smaller size and frame rate make a smaller GIF.",
                noun: "videos"))
        case .videoStills:
            .convert(ConvertPreset(
                kinds: ["video"], exts: ["gif"],
                target: .pick(kinds: ["image"], allowSame: false, preferred: ["jpg", "png"]), skipTargets: ["gif"],
                controls: [.time, .every, .resize], action: "Save Stills",
                dropTitle: "Drop a video or GIF",
                dropDetail: "With no time set, a video gives 10 stills spread across it and a GIF every frame. A start alone saves the one still there.",
                noun: "videos and GIFs"))
        case .extractAudio:
            .convert(ConvertPreset(
                kinds: ["video"], target: .pick(kinds: ["audio"], allowSame: false, preferred: ["mp3", "m4a"]),
                controls: [.time, .denoise], action: "Extract", dropTitle: "Drop videos to take the sound from",
                dropDetail: "The picture is left out.", noun: "videos"))
        case .addAudio:
            .convert(ConvertPreset(
                kinds: ["video"], target: .pick(kinds: ["video"], allowSame: true, preferred: []),
                controls: [.audio], action: "Add Audio", needs: "Choose the sound to put on it.",
                dropTitle: "Drop a video",
                dropDetail: "Then choose a song, a voice-over or another video. Its sound replaces the video's own, or plays under it.",
                noun: "videos"))
        case .burnSubtitles:
            .convert(ConvertPreset(
                kinds: ["video"], exts: ["gif"], target: .pick(kinds: ["video"], allowSame: true, preferred: []),
                controls: [.subtitles], action: "Burn Subtitles", needs: "Choose a subtitles file.",
                dropTitle: "Drop a video",
                dropDetail: "Then choose an .srt, .vtt or .ass file. White with an outline, near the bottom, sized to the picture.",
                noun: "videos and GIFs"))
        case .cropVideo:
            .convert(ConvertPreset(
                kinds: ["video"], exts: ["gif"], target: .pick(kinds: ["video"], allowSame: true, preferred: []),
                controls: [.crop], action: "Crop", needs: "Choose a shape or a box.",
                dropTitle: "Drop videos to crop",
                dropDetail: "Keep the middle at a shape like 9:16, or let conv cut away black bars.",
                noun: "videos and GIFs"))
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
                controls: [.rotate, .flip], action: "Turn", needs: "Choose a turn or a flip.",
                dropTitle: "Drop sideways or mirrored videos",
                dropDetail: "One that's wrong only because of its orientation tag is fixed without re-encoding.",
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
                dropDetail: "MP3, M4A, WAV, FLAC, AIFF, Opus and Ogg.", noun: "audio files"))
        case .denoise:
            .convert(ConvertPreset(
                kinds: ["audio", "video"], target: .pick(kinds: ["audio", "video"], allowSame: true, preferred: []),
                controls: [.denoise], startsOn: [.denoise], action: "Clean Up", needs: "Choose how much to take off.",
                dropTitle: "Drop recordings or videos",
                dropDetail: "For voice recordings, podcasts and phone videos. A video keeps its picture as it is; only the sound changes.",
                noun: "audio files and videos"))
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
        case .cropImage:
            .convert(ConvertPreset(
                kinds: ["image"], target: .pick(kinds: ["image"], allowSame: true, preferred: []),
                controls: [.crop], action: "Crop", needs: "Choose a shape or a box.",
                dropTitle: "Drop pictures to crop",
                dropDetail: "Keep the middle at a shape like 1:1, a box you give, or let conv trim a plain border.",
                noun: "pictures"))
        case .turnImage:
            .convert(ConvertPreset(
                kinds: ["image"], target: .pick(kinds: ["image"], allowSame: true, preferred: []),
                controls: [.rotate, .flip], action: "Turn", needs: "Choose a turn or a flip.",
                dropTitle: "Drop sideways or mirrored photos",
                dropDetail: "The picture itself is changed, so every app shows it the same way.",
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
        case .compressPDF:
            .convert(ConvertPreset(
                exts: ["pdf"], target: .keep, controls: [.compress, .maxSize], startsOn: [.compress],
                action: "Compress", needs: "Choose how to make it smaller.",
                dropTitle: "Drop PDFs to compress",
                dropDetail: "Big pictures are drawn at 150 dpi and saved again. Text, links, bookmarks and forms stay as they are, and the result is never bigger.",
                noun: "PDFs"))
        case .pdfToImages:
            .convert(ConvertPreset(
                exts: ["pdf"], target: .pick(kinds: ["image"], allowSame: false, preferred: ["png", "jpg"]),
                controls: [.pages, .dpi, .resize, .quality], action: "Save Pages",
                dropTitle: "Drop a PDF",
                dropDetail: "Every page, or the ones you pick, as pictures. Comments and filled-in forms show as they do in a viewer.",
                noun: "PDFs"))
        case .lockPDF:
            .convert(ConvertPreset(
                exts: ["pdf"], target: .keep, controls: [.lock], startsOn: [.lock], action: "Lock",
                needs: "Type a password.", dropTitle: "Drop PDFs to lock",
                dropDetail: "AES-256. The password never goes on a command line.", noun: "PDFs"))
        case .unlockPDF:
            .convert(ConvertPreset(
                exts: ["pdf"], target: .keep, controls: [.unlock], alwaysUnlock: true, action: "Unlock",
                needs: "conv can't unlock this file.", dropTitle: "Drop locked PDFs",
                dropDetail: "Writes a copy without the password and the print and copy restrictions. You need the password that opens it.",
                noun: "PDFs"))
        case .convertDocument:
            .convert(ConvertPreset(
                kinds: ["document"], skip: Self.dataExts,
                target: .pick(kinds: ["document"], allowSame: false, preferred: ["pdf", "docx"]),
                controls: [], dropTitle: "Drop documents to convert",
                dropDetail: "Word, Markdown, text, RTF, Excel, PowerPoint, OpenDocument and PDF.", noun: "documents"))
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
        case .mergePDF, .photosToPDF, .joinVideos, .joinAudio: .merge
        case .splitPDF: .split
        case .pdfPages: .pages
        case .scan: .scan
        case .qr: .qr
        case .ascii: .ascii
        case .formatData: .data
        case .readQR, .count, .changeCase, .cleanLinks, .findChars, .compareText, .lorem, .info,
             .base64, .hash, .jwt, .timestamp, .uuid: .quick
        case .speak: .speak
        case .wifi: .wifi
        case .beautify: .beautify
        case .colors: .colors
        case .makeArchive: .makeArchive
        case .openArchive: .openArchive
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

    /// Tools whose name is shared with one in another section.
    var sharesName: Bool { [.videoSpeed, .audioSpeed, .stripVideo, .stripAudio].contains(self) }
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
