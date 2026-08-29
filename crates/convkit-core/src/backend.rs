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
        }
    }

    /// Fallback remediation used when no supported package manager can be
    /// detected on PATH. Points at the official download page so `manual`
    /// is never left empty — an undetected package manager must never mean
    /// zero guidance.
    fn download_hint(&self) -> &'static str {
        match self {
            Backend::Ffmpeg | Backend::Ffprobe => {
                "install ffmpeg from https://ffmpeg.org/download.html"
            }
            Backend::Magick => {
                "install ImageMagick from https://imagemagick.org/script/download.php"
            }
            Backend::Soffice => "install LibreOffice from https://www.libreoffice.org/download/",
            Backend::Pandoc => "install pandoc from https://github.com/jgm/pandoc/releases",
        }
    }
}

impl ConvError {
    /// The manual-install command for `backend`: the right command for a
    /// detected package manager, or the official download page when none is
    /// detected — so this is never empty, regardless of what's on PATH.
    fn manual_hint_always_some(backend: Backend) -> String {
        PackageManager::detect()
            .map(|pm| backend.manual_hint(pm).to_string())
            .unwrap_or_else(|| backend.download_hint().to_string())
    }

    pub fn backend_missing(backend: Backend) -> ConvError {
        let managed = backend
            .is_managed()
            .then(|| format!("conv install {}", backend.exe_name()));
        ConvError {
            code: ErrorCode::BackendMissing,
            message: format!("{} not found", backend.exe_name()),
            backend: Some(backend),
            remediation: Some(Remediation {
                managed,
                manual: Some(Self::manual_hint_always_some(backend)),
            }),
        }
    }

    /// `conv install <backend>` refuses outright, for a backend where
    /// `is_managed()` is false. LibreOffice — today the only case — has no
    /// relocatable binary, so this is a permanent policy refusal, not a
    /// temporary gap: `remediation.managed` is left `None` on purpose,
    /// since offering `conv install <x>` as the fix for `conv install <x>`
    /// refusing would be circular.
    pub fn not_installable(backend: Backend) -> ConvError {
        ConvError {
            code: ErrorCode::BackendMissing,
            message: format!(
                "{} has no relocatable build; it can't be installed by conv",
                backend.exe_name()
            ),
            backend: Some(backend),
            remediation: Some(Remediation {
                managed: None,
                manual: Some(Self::manual_hint_always_some(backend)),
            }),
        }
    }

    /// `backend.is_managed()` is true, but `manifest::lookup` has no
    /// verified asset for the platform this process is running on.
    /// Deliberately distinct from an unverified manifest entry: per Task
    /// 14's controller ruling, a missing entry must fail immediately with
    /// this remediation, not after a download, with a checksum error the
    /// user cannot act on.
    pub fn no_managed_build(backend: Backend) -> ConvError {
        ConvError {
            code: ErrorCode::BackendMissing,
            message: format!(
                "no managed build of {} is available for this platform",
                backend.exe_name()
            ),
            backend: Some(backend),
            remediation: Some(Remediation {
                managed: None,
                manual: Some(Self::manual_hint_always_some(backend)),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_missing_never_leaves_remediation_empty() {
        // Soffice is never managed, and this must hold regardless of
        // whether a package manager happens to be installed on the machine
        // running this test.
        let e = ConvError::backend_missing(Backend::Soffice);
        let remediation = e
            .remediation
            .expect("backend_missing always sets remediation");
        assert_eq!(remediation.managed, None, "soffice is never managed");
        assert!(
            remediation.manual.is_some(),
            "manual must always be Some, even with no package manager detected"
        );
    }

    #[test]
    fn backend_missing_offers_managed_install_for_managed_backends() {
        let e = ConvError::backend_missing(Backend::Pandoc);
        let remediation = e
            .remediation
            .expect("backend_missing always sets remediation");
        assert_eq!(remediation.managed, Some("conv install pandoc".to_string()));
        assert!(remediation.manual.is_some());
    }

    #[test]
    fn not_installable_never_offers_a_managed_install() {
        let e = ConvError::not_installable(Backend::Soffice);
        assert_eq!(e.code, ErrorCode::BackendMissing);
        let remediation = e.remediation.expect("must carry remediation");
        assert_eq!(remediation.managed, None);
        assert!(remediation.manual.is_some());
    }

    #[test]
    fn no_managed_build_never_offers_a_managed_install_either() {
        let e = ConvError::no_managed_build(Backend::Pandoc);
        assert_eq!(e.code, ErrorCode::BackendMissing);
        let remediation = e.remediation.expect("must carry remediation");
        assert_eq!(
            remediation.managed, None,
            "offering `conv install pandoc` as the fix for `conv install pandoc` \
             finding no manifest entry would be circular"
        );
        assert!(remediation.manual.is_some());
    }
}
