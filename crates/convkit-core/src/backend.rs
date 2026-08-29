use serde::Serialize;

use crate::error::{ConvError, ErrorCode, Remediation};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Ffmpeg,
    /// Ships with ffmpeg; resolved separately because we invoke it directly.
    Ffprobe,
    Magick,
    Soffice,
    Pandoc,
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
        }
    }

    /// Whether `conv install <backend>` can provision this backend.
    /// LibreOffice has no relocatable binary and is therefore never managed.
    pub fn is_managed(&self) -> bool {
        !matches!(self, Backend::Soffice)
    }

    /// The command we print for the user to run. We never run it ourselves.
    pub fn manual_hint(&self, pm: PackageManager) -> &'static str {
        match (self, pm) {
            (Backend::Ffmpeg | Backend::Ffprobe, PackageManager::Winget) => "winget install Gyan.FFmpeg",
            (Backend::Ffmpeg | Backend::Ffprobe, PackageManager::Scoop) => "scoop install ffmpeg",
            (Backend::Ffmpeg | Backend::Ffprobe, PackageManager::Choco) => "choco install ffmpeg",
            (Backend::Ffmpeg | Backend::Ffprobe, PackageManager::Brew) => "brew install ffmpeg",
            (Backend::Ffmpeg | Backend::Ffprobe, PackageManager::Apt) => "sudo apt-get install ffmpeg",
            (Backend::Ffmpeg | Backend::Ffprobe, PackageManager::Dnf) => "sudo dnf install ffmpeg",
            (Backend::Ffmpeg | Backend::Ffprobe, PackageManager::Pacman) => "sudo pacman -S ffmpeg",

            (Backend::Magick, PackageManager::Winget) => "winget install ImageMagick.ImageMagick",
            (Backend::Magick, PackageManager::Scoop) => "scoop install imagemagick",
            (Backend::Magick, PackageManager::Choco) => "choco install imagemagick",
            (Backend::Magick, PackageManager::Brew) => "brew install imagemagick",
            (Backend::Magick, PackageManager::Apt) => "sudo apt-get install imagemagick",
            (Backend::Magick, PackageManager::Dnf) => "sudo dnf install ImageMagick",
            (Backend::Magick, PackageManager::Pacman) => "sudo pacman -S imagemagick",

            (Backend::Soffice, PackageManager::Winget) => "winget install TheDocumentFoundation.LibreOffice",
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
        }
    }
}

impl ConvError {
    pub fn backend_missing(backend: Backend) -> ConvError {
        let managed = backend
            .is_managed()
            .then(|| format!("conv install {}", backend.exe_name()));
        let manual = PackageManager::detect().map(|pm| backend.manual_hint(pm).to_string());
        ConvError {
            code: ErrorCode::BackendMissing,
            message: format!("{} not found", backend.exe_name()),
            backend: Some(backend),
            remediation: Some(Remediation { managed, manual }),
        }
    }
}
