use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Ffmpeg,
    /// Ships with ffmpeg; resolved separately because we invoke it directly.
    Ffprobe,
    Magick,
    Soffice,
    Pandoc,
    /// Typst, invoked as pandoc's `--pdf-engine`. Has a relocatable binary
    /// on every platform this manifest covers, so — unlike `Soffice` — it is
    /// managed.
    Typst,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManager {
    Winget,
    Choco,
    Scoop,
    Brew,
    Apt,
    Dnf,
    Pacman,
}

impl PackageManager {
    /// First package manager found on PATH, in preference order for the
    /// current platform. Used only to choose which install command to *print*.
    pub fn detect() -> Option<PackageManager> {
        const WINDOWS: &[(PackageManager, &str)] = &[
            (PackageManager::Winget, "winget"),
            (PackageManager::Scoop, "scoop"),
            (PackageManager::Choco, "choco"),
        ];
        const MACOS: &[(PackageManager, &str)] = &[(PackageManager::Brew, "brew")];
        const LINUX: &[(PackageManager, &str)] = &[
            (PackageManager::Apt, "apt-get"),
            (PackageManager::Dnf, "dnf"),
            (PackageManager::Pacman, "pacman"),
        ];

        let candidates = if cfg!(windows) {
            WINDOWS
        } else if cfg!(target_os = "macos") {
            MACOS
        } else {
            LINUX
        };

        candidates
            .iter()
            .find(|(_, exe)| which::which(exe).is_ok())
            .map(|(pm, _)| *pm)
    }
}

impl Backend {
    /// Executable stem. `which` appends the platform extension on Windows.
    pub fn exe_name(&self) -> &'static str {
        match self {
            Backend::Ffmpeg => "ffmpeg",
            Backend::Ffprobe => "ffprobe",
            Backend::Magick => "magick",
            Backend::Soffice => "soffice",
            Backend::Pandoc => "pandoc",
            Backend::Typst => "typst",
        }
    }

    /// Whether `conv install <backend>` can provision this backend.
    /// LibreOffice has no relocatable binary and is therefore never managed.
    pub fn is_managed(&self) -> bool {
        !matches!(self, Backend::Soffice)
    }

    /// The command we print for the user to run. We never run it ourselves.
    pub(crate) fn manual_hint(&self, pm: PackageManager) -> &'static str {
        match (self, pm) {
            (Backend::Ffmpeg | Backend::Ffprobe, PackageManager::Winget) => {
                "winget install Gyan.FFmpeg"
            }
            (Backend::Ffmpeg | Backend::Ffprobe, PackageManager::Scoop) => "scoop install ffmpeg",
            (Backend::Ffmpeg | Backend::Ffprobe, PackageManager::Choco) => "choco install ffmpeg",
            (Backend::Ffmpeg | Backend::Ffprobe, PackageManager::Brew) => "brew install ffmpeg",
            (Backend::Ffmpeg | Backend::Ffprobe, PackageManager::Apt) => {
                "sudo apt-get install ffmpeg"
            }
            (Backend::Ffmpeg | Backend::Ffprobe, PackageManager::Dnf) => "sudo dnf install ffmpeg",
            (Backend::Ffmpeg | Backend::Ffprobe, PackageManager::Pacman) => "sudo pacman -S ffmpeg",

            (Backend::Magick, PackageManager::Winget) => "winget install ImageMagick.ImageMagick",
            (Backend::Magick, PackageManager::Scoop) => "scoop install imagemagick",
            (Backend::Magick, PackageManager::Choco) => "choco install imagemagick",
            (Backend::Magick, PackageManager::Brew) => "brew install imagemagick",
            (Backend::Magick, PackageManager::Apt) => "sudo apt-get install imagemagick",
            (Backend::Magick, PackageManager::Dnf) => "sudo dnf install ImageMagick",
            (Backend::Magick, PackageManager::Pacman) => "sudo pacman -S imagemagick",

            (Backend::Soffice, PackageManager::Winget) => {
                "winget install TheDocumentFoundation.LibreOffice"
            }
            (Backend::Soffice, PackageManager::Scoop) => "scoop install libreoffice",
            (Backend::Soffice, PackageManager::Choco) => "choco install libreoffice-fresh",
            (Backend::Soffice, PackageManager::Brew) => "brew install --cask libreoffice",
            (Backend::Soffice, PackageManager::Apt) => "sudo apt-get install libreoffice",
            (Backend::Soffice, PackageManager::Dnf) => "sudo dnf install libreoffice",
            (Backend::Soffice, PackageManager::Pacman) => "sudo pacman -S libreoffice-fresh",

            (Backend::Pandoc, PackageManager::Winget) => "winget install JohnMacFarlane.Pandoc",
            (Backend::Pandoc, PackageManager::Scoop) => "scoop install pandoc",
            (Backend::Pandoc, PackageManager::Choco) => "choco install pandoc",
            (Backend::Pandoc, PackageManager::Brew) => "brew install pandoc",
            (Backend::Pandoc, PackageManager::Apt) => "sudo apt-get install pandoc",
            (Backend::Pandoc, PackageManager::Dnf) => "sudo dnf install pandoc",
            (Backend::Pandoc, PackageManager::Pacman) => "sudo pacman -S pandoc",

            // Typst has native packages in winget, scoop, brew, cargo (its
            // own crates.io release, `typst-cli`), and Arch's `extra` repo.
            // Neither Debian/Ubuntu nor Fedora carry an official package as
            // of this writing (verified against packages.debian.org and
            // Fedora's own package search — only unofficial COPR/AUR-style
            // repos have it), so those two fall back to the cargo install,
            // same binary either way.
            (Backend::Typst, PackageManager::Winget) => "winget install Typst.Typst",
            (Backend::Typst, PackageManager::Scoop) => "scoop install typst",
            (Backend::Typst, PackageManager::Choco) => "choco install typst",
            (Backend::Typst, PackageManager::Brew) => "brew install typst",
            (Backend::Typst, PackageManager::Apt) => "cargo install typst-cli",
            (Backend::Typst, PackageManager::Dnf) => "cargo install typst-cli",
            (Backend::Typst, PackageManager::Pacman) => "sudo pacman -S typst",
        }
    }

    /// The placeholder token `Step::render` emits for `Arg::BackendPath(_)`
    /// naming this backend, standing in for its resolved absolute path until
    /// `exec::run` substitutes the real one in at execution time.
    /// `plan::build` must stay pure — no filesystem access, no resolution —
    /// so it can never know the real path; this stays a fixed, readable
    /// string instead, purely a function of the backend, so `--dry-run`
    /// shows something a person can make sense of rather than opaque noise.
    /// Mirrors `plan::USER_INSTALLATION_PLACEHOLDER`'s role for the
    /// `Soffice`-specific `-env:UserInstallation` flag, generalised to any
    /// backend a recipe's own argv needs to name — see `Arg::BackendPath`'s
    /// docs for why the two are still separate mechanisms.
    pub fn path_placeholder(&self) -> String {
        format!("<resolved {} path>", self.exe_name())
    }

    /// Fallback remediation used when no supported package manager can be
    /// detected on PATH. Points at the official download page so `manual`
    /// is never left empty — an undetected package manager must never mean
    /// zero guidance.
    pub(crate) fn download_hint(&self) -> &'static str {
        match self {
            Backend::Ffmpeg | Backend::Ffprobe => {
                "install ffmpeg from https://ffmpeg.org/download.html"
            }
            Backend::Magick => {
                "install ImageMagick from https://imagemagick.org/script/download.php"
            }
            Backend::Soffice => "install LibreOffice from https://www.libreoffice.org/download/",
            Backend::Pandoc => "install pandoc from https://github.com/jgm/pandoc/releases",
            Backend::Typst => "install typst from https://github.com/typst/typst/releases",
        }
    }
}
