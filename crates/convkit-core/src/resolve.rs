use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::error::{ConvError, Result};
use crate::Backend;

/// Which of a small set of backends this process could actually resolve, as
/// of the moment `Resolver::check_availability` computed it. Exists solely
/// so `plan::build` can choose between two recipes for the same pair
/// (currently: soffice vs. pandoc+typst for docx/odt → pdf) without ever
/// touching the filesystem or spawning a process itself — `plan::build`
/// stays pure; only the caller (`exec::run`, `--dry-run`) ever asks a
/// `Resolver`, exactly the same split `Option<&MediaProbe>` already uses for
/// the auto-remux decision.
///
/// Built via `FromIterator<Backend>` (so `.collect()` works, including from
/// a test that wants an exact, deterministic combination with no `Resolver`
/// involved at all) or `Resolver::check_availability` (the real, I/O-backed
/// path).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AvailableBackends(HashSet<Backend>);

impl AvailableBackends {
    pub fn has(&self, backend: Backend) -> bool {
        self.0.contains(&backend)
    }
}

impl FromIterator<Backend> for AvailableBackends {
    fn from_iter<I: IntoIterator<Item = Backend>>(iter: I) -> Self {
        AvailableBackends(iter.into_iter().collect())
    }
}

/// Uniquifies the soffice version-probe's isolated profile directory across
/// however many probes happen to run inside one process (in practice at
/// most one per distinct `Resolver`, thanks to the cache below, but this
/// costs nothing and removes any doubt).
static VERSION_PROBE_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// How long a version probe (`version_of`/`probe_first_line`) waits for the
/// child to exit before giving up, killing it, and reporting the backend's
/// version as unknown. `Command::output()`/`wait()` has no built-in timeout,
/// so without this a backend that never exits -- most concretely, a
/// misresolved GUI-subsystem launcher like `soffice.exe` (see `well_known`'s
/// docs), which pops a console window and returns *before* doing any work,
/// leaving nothing else around to ever finish -- hangs the probe, and with
/// it `doctor`, `resolve`, and any test that resolves a backend, forever.
/// 5 seconds is already generous for a tool printing its own version; a
/// version probe slower than that is broken in its own right, so a timeout
/// here can never mistake a legitimately slow *conversion* for a hang --
/// conversion execution (`exec::run`) does not use this and is not subject
/// to it.
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the timeout loop below polls `Child::try_wait` while waiting
/// for a version probe to finish. Short enough that a real probe's result
/// is picked up promptly; long enough not to spin the CPU.
const VERSION_PROBE_POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Override,
    Env,
    Managed,
    Path,
    WellKnown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBackend {
    pub backend: Backend,
    pub path: PathBuf,
    pub version: String,
    pub source: Source,
}

#[derive(Debug, Default)]
pub struct Resolver {
    overrides: HashMap<Backend, PathBuf>,
    /// Escape hatch that suppresses `Source::WellKnown` entirely for this
    /// `Resolver` -- see `without_well_known`'s docs for why this exists
    /// and why it's a plain (always-compiled, not `#[cfg(test)]`) field:
    /// unlike every other candidate source, `WellKnown`'s fixed absolute
    /// paths have no override *value* that can suppress them. Since the
    /// override-authority fix (see `resolve()`'s docs), an `Override`/`Env`
    /// candidate that doesn't point at a real file is a hard, immediate
    /// error rather than a fall-through, so a deliberately-bad
    /// override/env value can no longer be used to "skip past" `WellKnown`
    /// either -- it never even reaches it. `Managed` and `Path` still fall
    /// through an absent candidate the same as always, but neither has a
    /// value a caller can point at a guaranteed-empty location the way
    /// `with_managed_dir`/a real, empty `PATH` do for the other two. So
    /// `WellKnown` remains the one candidate with no override *value* at
    /// all to suppress it: on a host with a real backend genuinely
    /// installed at its standard location, this field (via
    /// `without_well_known`) is the only thing that can make that backend
    /// deterministically unresolvable. Defaults to `false`; `conv`'s own
    /// `Cli::resolver()` never sets it, so production behaviour is
    /// unaffected.
    well_known_disabled: bool,
    /// Escape hatch that makes `candidates()` stop after `Source::Override`
    /// entirely, for this `Resolver` -- see `overrides_only`'s docs. A plain
    /// (always-compiled, not `#[cfg(test)]`) field for the same reason
    /// `well_known_disabled` is: `#[cfg(test)]` items in this crate are
    /// invisible outside it (cfg(test) only applies when *this* crate is the
    /// one being tested, never when it's compiled as an ordinary dependency
    /// for `conv`'s own tests), so a seam a dependent crate's tests must also
    /// reach can't be gated that way. Defaults to `false`; `conv`'s own
    /// `Cli::resolver()` never sets it, so production behaviour is
    /// unaffected.
    overrides_only: bool,
    /// Escape hatch from the real, machine-global `Resolver::managed_dir()`.
    /// Production code never sets this — see `candidates`'s use of
    /// `managed_dir_for` — so behaviour outside tests is unchanged. It
    /// exists because `%LOCALAPPDATA%\convkit\bin` (or its XDG equivalent)
    /// is real, shared, and — since Task 14 shipped `conv install` — can
    /// genuinely contain a real installed binary on whatever machine the
    /// test suite happens to run on; a test asserting "this backend
    /// resolves to nothing" (or "resolves to exactly this fixture") needs a
    /// managed dir it can guarantee is empty (or fully controlled), not the
    /// real one.
    ///
    /// A plain (always-compiled, not `#[cfg(test)]`) field, like
    /// `overrides_only`/`well_known_disabled` above, and for the identical
    /// reason: `conv`'s own `commands::update` tests need to isolate
    /// `resolve_managed_only` (F41's fix) from this machine's real managed
    /// dir too, and a `#[cfg(test)]` item in this crate is invisible when
    /// `convkit-core` is compiled as an ordinary dependency for `conv`'s own
    /// test build.
    managed_dir_override: Option<PathBuf>,
    /// Test-only override of where the ImageMagick-6 `convert` fallback
    /// (see `magick_convert_fallback`) looks for `convert`, in place of the
    /// real `PATH`. Same rationale as `managed_dir_override`: a contributor
    /// machine can genuinely have ImageMagick 6, or an unrelated `convert`,
    /// on its real `PATH`, so a deterministic test needs a search location
    /// it fully controls.
    #[cfg(test)]
    convert_search_dir_override: Option<PathBuf>,
    /// Test-only override of the version probe's timeout (see
    /// `VERSION_PROBE_TIMEOUT`), so a test proving the kill-on-timeout path
    /// itself (Fix 2) can use a deliberately-hung stub without burning the
    /// real 5-second production timeout on every run. Production code
    /// always uses `VERSION_PROBE_TIMEOUT`.
    #[cfg(test)]
    probe_timeout_override: Option<Duration>,
    /// Successful resolutions, keyed by backend. `Mutex` rather than
    /// `RefCell` because a `Resolver` is expected to be shared across
    /// threads in Task 12's rayon batch mode — plain interior mutability
    /// would make `Resolver` `!Sync` and fail to compile there.
    cache: Mutex<HashMap<Backend, ResolvedBackend>>,
}

impl Resolver {
    pub fn new() -> Self {
        Resolver::default()
    }

    pub fn with_override(&mut self, backend: Backend, path: PathBuf) {
        self.overrides.insert(backend, path);
    }

    /// Suppresses `Source::WellKnown` entirely for this `Resolver` --
    /// `resolve`/`candidates` will only ever consider override/env/managed/
    /// PATH afterward. `LibreOffice`'s Windows and macOS installers don't
    /// add `program\` (or the `.app`'s `MacOS/`) to `PATH`, so `WellKnown`
    /// is the *only* candidate that finds a standard install -- meaning on
    /// a host with a real, working LibreOffice there (an entirely ordinary
    /// state for a contributor to end up in, and not something this method
    /// is meant to work around outside of tests), nothing else can make
    /// `Backend::Soffice` deterministically unresolvable. This exists
    /// purely so a test needing exactly that can still be deterministic
    /// without depending on the host's real install state, or on
    /// destructively touching it. Not called anywhere outside tests --
    /// `conv`'s own `Cli::resolver()` never calls it -- so it changes
    /// nothing about how a real invocation resolves a backend.
    pub fn without_well_known(&mut self) {
        self.well_known_disabled = true;
    }

    /// Whether `WellKnown` candidates should be skipped for this `Resolver`:
    /// either `without_well_known` was called on it directly (the
    /// in-process case: convkit-core's own tests, and any dependent crate's
    /// tests, like `conv`'s unit tests, that construct a `Resolver`
    /// themselves), or the `CONVKIT_NO_WELL_KNOWN` environment variable is
    /// set. The env var covers the one case a per-instance flag can't: an
    /// integration test that drives the real, separately-spawned `conv`
    /// binary (`assert_cmd`) has no `Resolver` value to call a method on,
    /// only the child process's environment (`Command::env`) to set. Never
    /// read by `conv`'s own production code path -- only `Resolver` itself
    /// consults it -- and safe from the cross-test interference a global
    /// `std::env::set_var` in an in-process unit test would risk (Rust
    /// tests run in parallel threads within one process; each
    /// `assert_cmd::Command` instead spawns a genuinely separate child
    /// process with its own environment block, so setting this for one
    /// integration test can never affect another test running concurrently
    /// in the same suite).
    fn well_known_disabled(&self) -> bool {
        self.well_known_disabled || std::env::var_os("CONVKIT_NO_WELL_KNOWN").is_some()
    }

    /// Redirects the `Source::Managed` candidate to `dir` instead of the
    /// real `Resolver::managed_dir()`, so a test can assert "not found
    /// anywhere" without depending on this machine's real managed-install
    /// directory happening to be empty. Also what `resolve_managed_only`
    /// (F41's fix) reads, so a test can point it at a fixture managed dir
    /// too. `pub`, not `pub(crate)`, and not `#[cfg(test)]` -- see
    /// `managed_dir_override`'s own docs for why: `conv`'s own tests
    /// (`commands::update`) need this seam and cannot reach a `cfg(test)`
    /// item defined in this crate. Not called anywhere outside tests --
    /// `conv`'s own `Cli::resolver()` never calls it -- so production
    /// resolution is unaffected.
    pub fn with_managed_dir(&mut self, dir: PathBuf) {
        self.managed_dir_override = Some(dir);
    }

    /// Redirects where `magick_convert_fallback` looks for `convert` to
    /// `dir` instead of the real `PATH`, so a test can supply a stub
    /// `convert` (or none at all) deterministically. See
    /// `convert_search_dir_override`.
    #[cfg(test)]
    pub(crate) fn with_convert_search_dir(&mut self, dir: PathBuf) {
        self.convert_search_dir_override = Some(dir);
    }

    /// Redirects the version probe's timeout to `timeout` instead of the
    /// real `VERSION_PROBE_TIMEOUT`, so a test can prove a hung child gets
    /// killed without waiting the real 5 seconds. See
    /// `probe_timeout_override`.
    #[cfg(test)]
    pub(crate) fn with_probe_timeout(&mut self, timeout: Duration) {
        self.probe_timeout_override = Some(timeout);
    }

    /// Makes `candidates()` consult `Source::Override` only, for every
    /// backend, skipping `Env`, `Managed`, `Path`, and `WellKnown` entirely
    /// -- closing the whole candidate chain in one call rather than the two
    /// or three seams (`with_managed_dir`, `without_well_known`) a test
    /// would otherwise have to combine, and closing it further than they
    /// can reach at all: unlike those two, this also blocks `Source::Env`
    /// (`CONVKIT_<BACKEND>`) and `Source::Path`, both of which read this
    /// process's own real, global environment and neither of which any
    /// per-instance flag can suppress. Since the override-authority fix
    /// (see `resolve()`'s docs), an override pointing at a nonexistent path
    /// is a hard, immediate `InvalidInvocation` error rather than a
    /// fall-through -- so it's no longer even a candidate way to reach
    /// `Env`/`Path` by mistake -- but with *no* override set at all,
    /// `candidates()` still walks straight to them, exactly as it always
    /// has. A test asserting "this backend is absent" wants that as a
    /// property of the test itself, not of whether the machine it happens
    /// to run on has `CONVKIT_SOFFICE` set or a real backend on `PATH` --
    /// exactly the gap that let `fallback_recipe_substitutes_the_real_
    /// typst_path_and_never_touches_soffice` and `only_pandoc_available_
    /// still_reports_backend_missing_naming_soffice` in `exec.rs` pass on
    /// most machines and fail on the one that actually has LibreOffice on
    /// `PATH`.
    ///
    /// A backend a test *does* want found still needs an explicit
    /// `with_override` pointing at a real file -- this doesn't change how
    /// `Source::Override` itself is looked up, only what `candidates()`
    /// considers afterward.
    ///
    /// A plain (always-compiled, not `#[cfg(test)]`) method, like
    /// `without_well_known` and for the identical reason: `conv`'s own
    /// in-process unit tests (`commands::convert::tests` in particular)
    /// construct a `Resolver` directly the same way `convkit-core`'s own
    /// tests do, and a `#[cfg(test)]` item on this crate is invisible to
    /// them -- `cfg(test)` only applies when `convkit-core` itself is the
    /// crate under test, not when it's compiled as an ordinary dependency
    /// for `conv`'s test build. Not called anywhere outside tests -- `conv`'s
    /// own `Cli::resolver()` never calls it -- so production resolution is
    /// unaffected.
    pub fn overrides_only(&mut self) {
        self.overrides_only = true;
    }

    /// `probe_timeout_override` when a test has set one, otherwise the real
    /// `VERSION_PROBE_TIMEOUT`.
    fn probe_timeout(&self) -> Duration {
        #[cfg(test)]
        if let Some(timeout) = self.probe_timeout_override {
            return timeout;
        }
        VERSION_PROBE_TIMEOUT
    }

    /// Where `conv install` places managed binaries.
    pub fn managed_dir() -> PathBuf {
        #[cfg(windows)]
        let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
        #[cfg(not(windows))]
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));

        base.unwrap_or_else(std::env::temp_dir)
            .join("convkit")
            .join("bin")
    }

    /// `managed_dir_override` when a test has set one, otherwise the real
    /// `managed_dir()`. The only thing `candidates`/`resolve_managed_only`
    /// should call.
    fn managed_dir_for(&self) -> PathBuf {
        if let Some(dir) = &self.managed_dir_override {
            return dir.clone();
        }
        Self::managed_dir()
    }

    /// The platform-specific filename `Source::Managed` looks for —
    /// `<exe>.exe` on Windows, bare `<exe>` elsewhere. Factored out so
    /// `managed_path` and `candidates` share one spelling of this rule
    /// rather than each independently writing `if cfg!(windows) { ... }`
    /// (see Task 14 review finding 5: two independent copies is exactly the
    /// kind of thing that silently drifts).
    fn managed_filename(backend: Backend) -> String {
        let exe = backend.exe_name();
        if cfg!(windows) {
            format!("{exe}.exe")
        } else {
            exe.to_string()
        }
    }

    /// The exact file `Source::Managed` resolves `backend` to — and
    /// therefore the single source of truth for where `conv install` must
    /// write. Always the real, global `managed_dir()`; test isolation for
    /// *resolving* is `with_managed_dir`, but `conv install` itself always
    /// writes to the one real managed directory regardless of that.
    pub fn managed_path(backend: Backend) -> PathBuf {
        Self::managed_dir().join(Self::managed_filename(backend))
    }

    /// Environment variable consulted for this backend, e.g. `CONVKIT_FFMPEG`.
    fn env_var(backend: Backend) -> String {
        format!("CONVKIT_{}", backend.exe_name().to_ascii_uppercase())
    }

    /// Platform install locations checked last, for tools that commonly are not
    /// on PATH. LibreOffice on Windows and macOS is the main reason this exists.
    ///
    /// On Windows, LibreOffice ships three binaries in `program\`:
    /// `soffice.bin` (the real engine, never invoked directly), `soffice.com`
    /// (a console-subsystem launcher that blocks until the work is done and
    /// writes a capturable stdout), and `soffice.exe` (a GUI-subsystem
    /// launcher that pops its own console window and returns immediately,
    /// before the work finishes). Every one of `resolve`'s callers --
    /// `doctor`'s version probe, and a real conversion waiting for `soffice`
    /// to actually produce output -- needs the blocking, capturable one, so
    /// for each install location `.com` is listed before `.exe`; `resolve()`
    /// walks candidates in order and takes the first that exists, so `.exe`
    /// is only ever reached when that particular install genuinely lacks a
    /// `.com` (day-one LibreOffice on Windows always ships both, but this
    /// keeps a hypothetical partial install from resolving to nothing).
    fn well_known(backend: Backend) -> Vec<PathBuf> {
        match (backend, cfg!(windows), cfg!(target_os = "macos")) {
            (Backend::Soffice, true, _) => [
                r"C:\Program Files\LibreOffice\program",
                r"C:\Program Files (x86)\LibreOffice\program",
            ]
            .into_iter()
            .flat_map(|dir| {
                [
                    PathBuf::from(dir).join("soffice.com"),
                    PathBuf::from(dir).join("soffice.exe"),
                ]
            })
            .collect(),
            (Backend::Soffice, _, true) => {
                vec![PathBuf::from(
                    "/Applications/LibreOffice.app/Contents/MacOS/soffice",
                )]
            }
            _ => Vec::new(),
        }
    }

    /// Ordered candidates. Exposed for testing the documented precedence.
    pub fn candidates(&self, backend: Backend) -> Vec<(PathBuf, Source)> {
        let exe = backend.exe_name();
        let mut out = Vec::new();

        if let Some(p) = self.overrides.get(&backend) {
            out.push((p.clone(), Source::Override));
        }
        if self.overrides_only {
            return out;
        }
        if let Some(p) = std::env::var_os(Self::env_var(backend)) {
            out.push((PathBuf::from(p), Source::Env));
        }
        let managed = self.managed_dir_for().join(Self::managed_filename(backend));
        out.push((managed, Source::Managed));
        if let Ok(p) = Self::which_backend(backend, exe) {
            out.push((p, Source::Path));
        }
        if !self.well_known_disabled() {
            out.extend(
                Self::well_known(backend)
                    .into_iter()
                    .map(|p| (p, Source::WellKnown)),
            );
        }
        out
    }

    /// `PATH` lookup for `backend`. Ordinary `which::which(exe)` for every
    /// backend except `Soffice` on Windows: `which("soffice")` there has no
    /// extension of its own, so `which` appends one by walking `PATHEXT` in
    /// whatever order the user's environment defines it -- normally
    /// `.COM` before `.EXE`, but that is a user-controlled setting, not a
    /// guarantee, and picking `.exe` is exactly the bug this exists to
    /// prevent (see `well_known`'s docs on why `.com` is required). This
    /// probes `soffice.com` explicitly first, falling back to `soffice.exe`
    /// only when no `.com` is on `PATH` at all -- the same precedence
    /// `well_known` uses for the fixed install locations, applied to `PATH`
    /// too, per the fix brief's "do the same anywhere else a soffice path
    /// is constructed or discovered, including the PATH lookup."
    fn which_backend(backend: Backend, exe: &str) -> std::result::Result<PathBuf, which::Error> {
        if cfg!(windows) && backend == Backend::Soffice {
            which::which("soffice.com").or_else(|_| which::which("soffice.exe"))
        } else {
            which::which(exe)
        }
    }

    /// Resolves a backend, caching the result so each backend is probed —
    /// meaning: its candidates walked and, on a hit, its version probe spawned
    /// — at most once per `Resolver` (in practice, once per process, since
    /// one `Resolver` is expected to live for a whole `conv` invocation).
    /// Only successes are cached; a missing backend is cheap to re-check
    /// (no subprocess involved) and re-checking costs nothing.
    pub fn resolve(&self, backend: Backend) -> Result<ResolvedBackend> {
        if let Some(cached) = self.cache.lock().unwrap().get(&backend) {
            return Ok(cached.clone());
        }
        for (path, source) in self.candidates(backend) {
            if !path.is_file() {
                match source {
                    // `Source::Override` (an explicit `--<backend>-path`)
                    // and `Source::Env` (`CONVKIT_<BACKEND>`) are the user
                    // (or their environment) *asserting* "use exactly this
                    // one" -- unlike every candidate after it, there is no
                    // sense in which silently trying something else honours
                    // that assertion, only in which it overrides it. A
                    // path that doesn't exist there is a hard, immediate
                    // error naming the flag/variable and the path given,
                    // never a fall-through to the next candidate. This is
                    // the fix for the live defect: `--ffprobe-path
                    // /definitely/not/here` used to be skipped exactly like
                    // a missing `Source::Managed` candidate, so probing
                    // silently proceeded with a different, unrequested
                    // ffprobe instead of failing loudly. See this fix's
                    // report for the full reasoning, including why `Env`
                    // errors rather than warns-and-continues.
                    Source::Override => {
                        let label = format!("--{}-path", backend.exe_name());
                        return Err(ConvError::invalid_backend_override(backend, &label, &path));
                    }
                    Source::Env => {
                        let label = Self::env_var(backend);
                        return Err(ConvError::invalid_backend_override(backend, &label, &path));
                    }
                    // `Source::Managed`, `Source::Path`, and
                    // `Source::WellKnown` are *discovery*, not assertion --
                    // nobody named this exact path, so an absent candidate
                    // here is unremarkable and falling through to the next
                    // one is exactly right. Unchanged from before this fix.
                    Source::Managed | Source::Path | Source::WellKnown => continue,
                }
            }
            let version = Self::version_of(backend, &path, self.probe_timeout())
                .unwrap_or_else(|| "unknown".into());
            let resolved = ResolvedBackend {
                backend,
                path,
                version,
                source,
            };
            self.cache.lock().unwrap().insert(backend, resolved.clone());
            return Ok(resolved);
        }
        // No `magick` found anywhere in the candidate chain above. Ubuntu's
        // (and Debian's) `apt-get install imagemagick` — the exact command
        // `conv doctor`/`backend_missing` advise — installs ImageMagick 6,
        // whose unified binary is `convert`, not `magick`; `magick` only
        // exists in ImageMagick 7. `Self::magick_convert_fallback_applies`
        // is `false` on Windows, where `convert.exe` is the OS's own
        // destructive FAT->NTFS conversion tool, not ImageMagick — the very
        // reason ImageMagick renamed its unified binary. See
        // `magick_convert_fallback`'s docs for the acceptance check.
        if Self::magick_convert_fallback_applies(backend, cfg!(windows)) {
            if let Some(resolved) = self.magick_convert_fallback() {
                self.cache.lock().unwrap().insert(backend, resolved.clone());
                return Ok(resolved);
            }
        }
        Err(ConvError::backend_missing(backend))
    }

    /// Probes only the `Source::Managed` candidate for `backend` -- the
    /// exact file `conv install`/`conv update` itself would write to
    /// (`managed_dir_for().join(managed_filename(backend))`) -- ignoring
    /// `Override`, `Env`, `Path`, and `WellKnown` entirely, and never
    /// touching the shared `resolve()` cache.
    ///
    /// This is what `conv update`'s classification must call instead of
    /// the general `resolve()` (review finding F41): a copy resolved from
    /// anywhere else in the chain -- a newer system ffmpeg from Homebrew on
    /// `PATH`, an explicit `--ffmpeg-path`/`CONVKIT_FFMPEG` override -- is
    /// not something convkit itself manages. Judging *that* copy against
    /// the pin used to mean a newer, perfectly good system install was
    /// labelled "outdated" and then silently shadowed by a downloaded,
    /// older pinned build, which then outranks `PATH` on every later run
    /// (`Source::Managed` beats `Source::Path`; see `candidates`'s
    /// ordering) -- and with an override set, the override is what got
    /// judged, so `--check` could never turn green at all.
    ///
    /// Returns `None` when no file exists at the managed location -- not
    /// an error. Unlike `resolve()`, an absent managed copy is an entirely
    /// unremarkable, common state (a backend simply never installed, or
    /// intentionally left to a copy elsewhere), so this reports it the same
    /// way `is_file()` would: as a plain boolean-shaped absence, not a
    /// `ConvError` a caller has to unwrap past. `conv update`'s own
    /// `classify_managed` is the one place this distinction matters (review
    /// finding F42): "not installed" and "genuinely broken" are different
    /// states with different exit codes, and only this method's caller
    /// knows which one applies.
    pub fn resolve_managed_only(&self, backend: Backend) -> Option<ResolvedBackend> {
        let path = self.managed_dir_for().join(Self::managed_filename(backend));
        if !path.is_file() {
            return None;
        }
        let version = Self::version_of(backend, &path, self.probe_timeout())
            .unwrap_or_else(|| "unknown".into());
        Some(ResolvedBackend {
            backend,
            path,
            version,
            source: Source::Managed,
        })
    }

    /// Resolves exactly `candidates`, recording which ones succeeded, so
    /// `plan::build` can be handed the answer instead of resolving anything
    /// itself. Never probes a backend outside `candidates` — callers pass
    /// only the small set relevant to the pair being converted (see
    /// `registry::FALLBACK_BACKENDS`), gated on `registry::has_fallback` so
    /// an ordinary conversion with only one possible recipe never pays for
    /// a version probe it has no use for. Resolutions are cached the same
    /// way `resolve` always caches them, so a backend checked here and later
    /// actually used (e.g. the chosen step's own backend) is never probed
    /// twice.
    pub fn check_availability(&self, candidates: &[Backend]) -> AvailableBackends {
        candidates
            .iter()
            .copied()
            .filter(|b| self.resolve(*b).is_ok())
            .collect()
    }

    /// Whether the ImageMagick-6 `convert` fallback should even be
    /// attempted for `(backend, is_windows)`. Takes `is_windows` as an
    /// explicit argument, rather than calling `cfg!(windows)` internally,
    /// so this predicate — unlike the real call site in `resolve`, which
    /// always passes the real `cfg!(windows)` — is itself testable for both
    /// branches on every host in the CI matrix, including windows-latest,
    /// where `cfg!(windows)` is always `true` and could otherwise never
    /// exercise the `false` branch at all.
    fn magick_convert_fallback_applies(backend: Backend, is_windows: bool) -> bool {
        backend == Backend::Magick && !is_windows
    }

    /// The ImageMagick-6 `convert` fallback for `Backend::Magick`. Callers
    /// (just `resolve`) must gate this on
    /// `magick_convert_fallback_applies(..., cfg!(windows))` themselves —
    /// this method has no platform check of its own, so tests can exercise
    /// its acceptance logic directly on any host, including a Windows dev
    /// machine, where the real call site never reaches it.
    ///
    /// `convert` is a generic enough name that something else on `PATH`
    /// could easily answer to it (a different tool entirely, or — though
    /// never reached here, since the call site gates this off on Windows —
    /// the OS's own FAT->NTFS converter), so unlike every other candidate
    /// in `candidates()`, existing as a file is not enough: this runs the
    /// same version probe `version_of` uses and requires the banner to
    /// actually contain `ImageMagick` before accepting it.
    fn magick_convert_fallback(&self) -> Option<ResolvedBackend> {
        let path = self.convert_candidate_path()?;
        if !path.is_file() || !Self::looks_like_imagemagick(&path, self.probe_timeout()) {
            return None;
        }
        let version = Self::version_of(Backend::Magick, &path, self.probe_timeout())
            .unwrap_or_else(|| "unknown".into());
        Some(ResolvedBackend {
            backend: Backend::Magick,
            path,
            version,
            source: Source::Path,
        })
    }

    /// Where the `convert` fallback looks for its candidate: the real
    /// `PATH` in production, or `convert_search_dir_override` under test.
    fn convert_candidate_path(&self) -> Option<PathBuf> {
        #[cfg(test)]
        if let Some(dir) = &self.convert_search_dir_override {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            return which::which_in("convert", Some(dir.as_os_str()), cwd).ok();
        }
        which::which("convert").ok()
    }

    /// Runs `convert -version` and checks whether the banner mentions
    /// `ImageMagick` at all — ImageMagick 6's `convert -version` banner has
    /// the same "Version: ImageMagick 6.9.x ..." shape as `magick
    /// -version`'s. A `convert` that isn't ImageMagick (some unrelated
    /// program sharing the name) has no reason to print that word, so this
    /// rejects it rather than adopting it as the image backend.
    fn looks_like_imagemagick(path: &Path, timeout: Duration) -> bool {
        Self::probe_first_line(Backend::Magick, path, "-version", timeout)
            .is_some_and(|line| line.contains("ImageMagick"))
    }

    /// A short version token extracted from the first line of the
    /// backend's version banner (see `extract_version_token`), not the
    /// whole line. Never fails the resolve — an unreadable version is
    /// reported as "unknown".
    ///
    /// Two things a naive `--version` probe gets wrong, discovered by
    /// `conv doctor` reporting a real, working ffmpeg as "unknown":
    /// `--version` is not a real ffmpeg (or ffprobe, or ImageMagick
    /// `magick`) option at all — they take the single-dash `-version` —
    /// and ffmpeg's version banner is written to stderr regardless of which
    /// flag is used, leaving stdout empty. pandoc and soffice use the
    /// conventional double-dash `--version` and do print it to stdout, so
    /// this only falls back to stderr when stdout came back empty rather
    /// than always preferring one stream.
    ///
    /// For `soffice` specifically, this is itself a soffice invocation, so
    /// it gets its own isolated `-env:UserInstallation` profile just like a
    /// real conversion does — the constraint that every soffice invocation
    /// gets its own profile has no carve-out for version probes, and without
    /// this a probe could collide with an already-running LibreOffice. The
    /// probe's profile directory is removed afterward on a best-effort
    /// basis; nothing downstream depends on it surviving.
    fn version_of(backend: Backend, path: &Path, timeout: Duration) -> Option<String> {
        let flag = match backend {
            Backend::Ffmpeg | Backend::Ffprobe | Backend::Magick => "-version",
            Backend::Pandoc | Backend::Soffice | Backend::Typst => "--version",
        };
        let first_line = Self::probe_first_line(backend, path, flag, timeout)?;
        extract_version_token(&first_line).map(str::to_string)
    }

    /// Runs `path` with `flag` and returns the first line of its version
    /// banner (stdout, falling back to stderr when stdout came back empty —
    /// see `version_of`'s docs on why). Factored out of `version_of` so
    /// `looks_like_imagemagick` can inspect the same raw banner text
    /// without needing `extract_version_token`'s short digit-bearing token,
    /// which for a real ImageMagick banner ("Version: ImageMagick 6.9.11-60
    /// ...") is just "6.9.11-60" — exactly the version number, and
    /// therefore exactly the substring that does *not* contain the literal
    /// word "ImageMagick" this needs to check for.
    fn probe_first_line(
        backend: Backend,
        path: &Path,
        flag: &str,
        timeout: Duration,
    ) -> Option<String> {
        // Windows console-window suppression (`CREATE_NO_WINDOW`) is applied
        // inside `backend_command`, not repeated here -- see its docs. This
        // is what stops soffice's own version probe (and the ImageMagick-6
        // `convert` identification probe in `looks_like_imagemagick`, which
        // also routes through here) from popping a console window -- worse,
        // one stuck reading "Press Enter to continue..." -- on every resolve.
        let mut cmd = crate::procutil::backend_command(path);
        cmd.arg(flag);

        let profile = (backend == Backend::Soffice).then(|| {
            std::env::temp_dir().join(format!(
                "convkit-lo-version-probe-{}-{}",
                std::process::id(),
                VERSION_PROBE_COUNTER.fetch_add(1, Ordering::Relaxed)
            ))
        });
        if let Some(profile) = &profile {
            if let Ok(url) = crate::exec::user_installation_url(profile) {
                cmd.arg(format!("-env:UserInstallation={url}"));
            }
        }

        let out = Self::run_with_timeout(cmd, timeout);
        if let Some(profile) = &profile {
            let _ = std::fs::remove_dir_all(profile);
        }
        let out = out?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let text = if stdout.trim().is_empty() {
            String::from_utf8_lossy(&out.stderr).into_owned()
        } else {
            stdout.into_owned()
        };
        text.lines().next().map(str::to_string)
    }

    /// `Command::output()` with a deadline, so a version probe can never
    /// hang the process. No timeout API exists on `std::process::Command`
    /// (this is exactly the deferred finding this fix promotes), so this
    /// hand-rolls one without a heavier dependency: spawn the child with its
    /// stdout/stderr piped, poll `Child::try_wait` on a short interval until
    /// either it exits or `timeout` elapses, and on the latter, `kill` it.
    ///
    /// Returns `None` on spawn failure, on timeout, or if reaping/collecting
    /// the child's output ever fails -- `version_of`'s caller already treats
    /// `None` as "report unknown," so a timeout needs no separate case.
    ///
    /// On a timeout, the child is both killed *and* waited on afterward
    /// (`Child::wait`, not just `kill`), so it is actually reaped rather
    /// than left as a zombie (Unix) or a stray process/console window
    /// (Windows) -- "kill" alone only requests termination, it does not
    /// collect the exit status.
    fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Option<std::process::Output> {
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().ok()?;

        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(_status)) => break,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return None;
                    }
                    std::thread::sleep(VERSION_PROBE_POLL_INTERVAL);
                }
                Err(_) => return None,
            }
        }

        child.wait_with_output().ok()
    }
}

/// Picks the first whitespace-separated token on `line` that contains a
/// digit, rather than returning the whole banner line. A naive whole-line
/// return blew out `doctor`'s tabular version column — ffmpeg's own first
/// line alone runs to roughly 90 characters — so this extracts a short,
/// version-ish token instead. Works across every backend's banner shape
/// without per-backend parsing, since each one puts a short version-like
/// token near the front of its first line:
///   "ffmpeg version 9.0-full_build-www.gyan.dev Copyright (c) ..." -> "9.0-full_build-www.gyan.dev"
///   "Version: ImageMagick 7.1.1-29 Q16-HDRI x64 ..."                -> "7.1.1-29"
///   "pandoc 3.1.11"                                                 -> "3.1.11"
///   "LibreOffice 7.6.4.1"                                           -> "7.6.4.1"
/// Returns `None` when no token on the line contains a digit at all, so
/// the caller falls back to "unknown" rather than printing garbage.
fn extract_version_token(line: &str) -> Option<&str> {
    line.split_whitespace()
        .find(|tok| tok.chars().any(|c| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn an_override_wins_over_everything_else() {
        let mut r = Resolver::new();
        let fake = PathBuf::from("/nowhere/custom-ffmpeg");
        r.with_override(Backend::Ffmpeg, fake.clone());
        let got = r.candidates(Backend::Ffmpeg);
        assert_eq!(got[0].0, fake);
        assert_eq!(got[0].1, Source::Override);
    }

    /// Review finding 5: `conv install` writes to `Resolver::managed_path`,
    /// and `resolve()` looks for the backend at the `Source::Managed`
    /// candidate `candidates()` produces — these must always agree on the
    /// filename, or `conv install` could report success while `doctor`
    /// reports the backend still missing, with nothing failing to say so.
    #[test]
    fn managed_path_matches_the_managed_candidate_filename() {
        let r = Resolver::new();
        for backend in [
            Backend::Ffmpeg,
            Backend::Ffprobe,
            Backend::Magick,
            Backend::Pandoc,
            Backend::Soffice,
            Backend::Typst,
        ] {
            let managed_candidate = r
                .candidates(backend)
                .into_iter()
                .find(|(_, source)| *source == Source::Managed)
                .expect("every backend has a Managed candidate")
                .0;
            assert_eq!(
                managed_candidate.file_name(),
                Resolver::managed_path(backend).file_name(),
                "{backend:?}: managed_path and the Managed candidate disagree on filename"
            );
        }
    }

    // --- F41: `resolve_managed_only` probes *only* the managed slot -------

    /// Writes a stub at `dir/name` (choosing the OS-appropriate script
    /// extension itself) whose `-version`/`--version` banner echoes
    /// exactly `name version` -- any filename is fine here, since this is
    /// only ever used for `Source::Override`, which accepts one. Mirrors
    /// the identically-named helper in `commands::update`'s own tests
    /// (`conv`, a separate crate): the same tradeoff `stub_that_echoes_
    /// first_arg` above already makes of duplicating a small stub-builder
    /// rather than sharing it across the crate boundary.
    fn stub_with_version(dir: &Path, name: &str, version: &str) -> PathBuf {
        let (file_name, body): (String, String) = if cfg!(windows) {
            (
                format!("{name}.bat"),
                format!("@echo off\r\necho {name} {version}\r\nexit /b 0\r\n"),
            )
        } else {
            (
                name.to_string(),
                format!("#!/bin/sh\necho \"{name} {version}\"\n"),
            )
        };
        let p = dir.join(file_name);
        std::fs::write(&p, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    #[test]
    fn resolve_managed_only_returns_none_when_the_managed_dir_is_empty() {
        let empty = tempfile::tempdir().unwrap();
        let mut r = Resolver::new();
        r.with_managed_dir(empty.path().to_path_buf());
        assert!(r.resolve_managed_only(Backend::Typst).is_none());
    }

    /// Unlike `Source::Override` (any filename accepted, see
    /// `stub_with_version` above), the `Managed` candidate is a fixed,
    /// platform-specific filename (`managed_filename`) -- `typst.exe` on
    /// Windows, bare `typst` elsewhere. Unix-only: `CreateProcess` decides
    /// how to run a file from its literal extension, and unlike `.bat`/
    /// `.cmd` (delegated to `cmd.exe`), a `.exe` file is loaded as a native
    /// PE image, so a plain-text stub named `typst.exe` fails to spawn at
    /// all rather than running as a script. Fabricating a real minimal PE
    /// binary from a test fixture is out of proportion to what this test
    /// needs to prove; `resolve_managed_only_returns_none_when_the_managed_
    /// dir_is_empty` above and `managed_path_matches_the_managed_candidate_
    /// filename` already exercise this method's filename logic on every
    /// platform, just not end-to-end through a real spawned version probe.
    #[cfg(unix)]
    #[test]
    fn resolve_managed_only_finds_a_file_written_at_the_managed_path() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(Resolver::managed_filename(Backend::Typst));
        std::fs::write(&p, "#!/bin/sh\necho \"0.15.1\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut r = Resolver::new();
        r.with_managed_dir(dir.path().to_path_buf());

        let resolved = r
            .resolve_managed_only(Backend::Typst)
            .expect("a file at the managed path must resolve");
        assert_eq!(resolved.source, Source::Managed);
        assert_eq!(resolved.version, "0.15.1");
    }

    /// The mechanism finding F41 exists to fix: an override (standing in
    /// for `--<backend>-path`/`CONVKIT_<BACKEND>`, or a real system PATH
    /// install) must never be consulted by `resolve_managed_only`, even
    /// though the *general* `resolve()` would find it immediately (Override
    /// is the very first candidate in the chain). The managed dir here is
    /// isolated and empty, so if this returned `Some` at all, it could only
    /// be by way of the override -- exactly the leak this method exists to
    /// close.
    #[test]
    fn resolve_managed_only_ignores_an_override_even_though_resolve_would_use_it() {
        let empty_managed_dir = tempfile::tempdir().unwrap();
        let external_dir = tempfile::tempdir().unwrap();
        let stub = stub_with_version(external_dir.path(), "typst", "9.9.9");
        let mut r = Resolver::new();
        r.with_managed_dir(empty_managed_dir.path().to_path_buf());
        r.with_override(Backend::Typst, stub.clone());

        assert!(r.resolve_managed_only(Backend::Typst).is_none());
        // Confirms the setup: the general chain *does* find it via the
        // override, so the `None` above is `resolve_managed_only` actively
        // ignoring it, not an accident of a broken stub.
        let general = r.resolve(Backend::Typst).unwrap();
        assert_eq!(general.source, Source::Override);
        assert_eq!(general.path, stub);
    }

    #[test]
    fn candidate_order_matches_the_spec() {
        let r = Resolver::new();
        let sources: Vec<Source> = r
            .candidates(Backend::Ffmpeg)
            .into_iter()
            .map(|c| c.1)
            .collect();
        let expected = [
            Source::Env,
            Source::Managed,
            Source::Path,
            Source::WellKnown,
        ];
        let filtered: Vec<Source> = sources
            .into_iter()
            .filter(|s| expected.contains(s))
            .collect();
        let mut seen_order: Vec<Source> = Vec::new();
        for s in filtered {
            if seen_order.last() != Some(&s) {
                seen_order.push(s);
            }
        }
        for w in seen_order.windows(2) {
            let a = expected.iter().position(|e| *e == w[0]).unwrap();
            let b = expected.iter().position(|e| *e == w[1]).unwrap();
            assert!(a < b, "candidate sources out of order: {seen_order:?}");
        }
    }

    /// I6: `with_override` points at a file that's guaranteed not to exist,
    /// and `with_managed_dir` redirects the `Managed` candidate to an empty
    /// tempdir this test owns. That much still proves the `candidates()`
    /// shape below deterministically. `resolve()` itself, though, is no
    /// longer exercised through this same `Resolver`: since the
    /// override-authority fix (a `Source::Override` path that doesn't exist
    /// is now a hard, immediate `InvalidInvocation` error, never a
    /// fall-through — see `Resolver::resolve`'s docs), the bogus override
    /// above would make `resolve()` report *that*, not `backend_missing`,
    /// without ever reaching `Env`/`Managed`/`Path`/`WellKnown` — not what
    /// this test is about. The "a genuinely missing backend produces a
    /// remediable `backend_missing` error" half below therefore uses a
    /// second, `overrides_only()` resolver with *no* override set for
    /// Pandoc at all: deterministic on every machine regardless of whether
    /// pandoc happens to be on `PATH`/`CONVKIT_PANDOC`/installed via `conv
    /// install`, closing the exact host-dependence the old version of this
    /// test had to work around with a conditional `if let Err(e) = ...`
    /// (an even older version asserted `.unwrap_err()` unconditionally and
    /// broke on any contributor machine with pandoc genuinely installed).
    #[test]
    fn a_missing_backend_produces_a_remediable_error() {
        let empty_managed_dir = tempfile::tempdir().unwrap();
        let mut r = Resolver::new();
        r.with_managed_dir(empty_managed_dir.path().to_path_buf());
        let bogus = PathBuf::from("/definitely/not/here");
        r.with_override(Backend::Pandoc, bogus.clone());

        let candidates = r.candidates(Backend::Pandoc);
        assert_eq!(
            candidates[0],
            (bogus.clone(), Source::Override),
            "the override must be the first candidate"
        );
        assert!(!bogus.is_file(), "the override path must not exist");
        let managed = candidates
            .iter()
            .find(|(_, s)| *s == Source::Managed)
            .expect("every backend has a Managed candidate");
        assert!(
            managed.0.starts_with(empty_managed_dir.path()),
            "the Managed candidate must use the isolated tempdir, not the \
             real machine-global managed dir"
        );
        assert!(!managed.0.is_file(), "the isolated managed dir is empty");

        let mut genuinely_missing = Resolver::new();
        genuinely_missing.overrides_only();
        // No override set for Pandoc: candidates() is empty deterministically,
        // regardless of this host's real PATH/CONVKIT_PANDOC/managed dir --
        // see overrides_only's docs.
        let e = genuinely_missing.resolve(Backend::Pandoc).unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::BackendMissing);
        let rem = e.remediation.expect("must carry remediation");
        if crate::manifest::has_managed_build(Backend::Pandoc) {
            assert_eq!(rem.managed.as_deref(), Some("conv install pandoc"));
        } else {
            // No manifest row for this platform (e.g. linux/arm64) -- no
            // managed install can genuinely be offered.
            assert_eq!(rem.managed, None);
            assert!(rem.manual.is_some());
        }
    }

    /// What actually guarantees LibreOffice is never offered as a managed
    /// install is `Backend::is_managed()` being `false` for it — a static
    /// predicate `ConvError::backend_missing` reads before it ever looks at
    /// `candidates()` — asserted directly and unconditionally here (the
    /// same, zero-I/O guarantee `error.rs`'s own
    /// `backend_missing_never_leaves_remediation_empty` proves
    /// independently, with no `Resolver` involved at all).
    ///
    /// The `Resolver`-based half below used to point a bogus
    /// `--soffice-path` override at a nonexistent file and rely on it
    /// falling through to `Source::Env`/`Source::Path` — best-effort, since
    /// an in-process unit test can't redirect this process's own real
    /// `PATH`/`CONVKIT_SOFFICE` the way a spawned child's can (see `cli.rs`'s
    /// `command_with_no_backends`, which uses `env_clear()`). Since the
    /// override-authority fix (a `Source::Override` path that doesn't exist
    /// is now a hard, immediate `InvalidInvocation` error, never a
    /// fall-through), that bogus override no longer reaches
    /// `backend_missing` at all — so this now uses `overrides_only()` with
    /// *no* override for Soffice instead, which makes `candidates()` empty
    /// deterministically (see `overrides_only`'s docs) and exercises the
    /// genuine `backend_missing` path this test is actually about,
    /// unconditionally rather than best-effort.
    #[test]
    fn libreoffice_is_never_offered_as_a_managed_install() {
        assert!(!Backend::Soffice.is_managed());

        let mut r = Resolver::new();
        r.overrides_only();
        let e = r.resolve(Backend::Soffice).unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::BackendMissing);
        assert_eq!(e.remediation.unwrap().managed, None);
    }

    // --- Controller review round 3: version_of used the wrong flag and
    // ignored stderr, so a real ffmpeg was reported as "unknown" -------------

    /// Writes a script that echoes its *first* argument, with a `-9`
    /// suffix, to stdout and exits 0, so a test can observe exactly which
    /// version flag `version_of` passed. Must be the first argument, not
    /// the last: for `soffice`, `version_of` appends
    /// `-env:UserInstallation=...` *after* the version flag, so the flag is
    /// not reliably the last argument, only the first. The `-9` suffix
    /// matters because `version_of` now extracts a digit-bearing token from
    /// the banner line rather than returning it whole (see
    /// `extract_version_token`); a bare `-version`/`--version` has no digit
    /// in it, so without the suffix this stub would make every case report
    /// "unknown" regardless of which flag was passed.
    fn stub_that_echoes_first_arg(dir: &Path) -> PathBuf {
        let (name, body) = if cfg!(windows) {
            ("echo_first.bat", "@echo off\r\necho %~1-9\r\nexit /b 0\r\n")
        } else {
            ("echo_first.sh", "#!/bin/sh\necho \"$1-9\"\n")
        };
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    /// Writes a script that prints fixed text to *stderr only* and exits 0 —
    /// mirrors a real ffmpeg build, whose `--version`/`-version` banner never
    /// touches stdout at all.
    fn stub_that_prints_version_to_stderr_only(dir: &Path) -> PathBuf {
        let (name, body) = if cfg!(windows) {
            (
                "stderr_version.bat",
                "@echo off\r\n\
                 echo banner-9.0 1>&2\r\n\
                 exit /b 0\r\n",
            )
        } else {
            (
                "stderr_version.sh",
                "#!/bin/sh\n\
                 echo \"banner-9.0\" >&2\n",
            )
        };
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    /// Writes a script named `convert` (ImageMagick 6's unified-binary name
    /// — the fallback always looks it up by this exact name) whose
    /// `-version` banner has the same shape as a real ImageMagick 6
    /// install's, so `magick_convert_fallback`'s ImageMagick check accepts
    /// it.
    fn stub_convert_that_is_really_imagemagick(dir: &Path) -> PathBuf {
        let (name, body) = if cfg!(windows) {
            (
                "convert.bat",
                "@echo off\r\n\
                 echo Version: ImageMagick 6.9.11-60 Q16 x86_64 20200481 https://imagemagick.org\r\n\
                 exit /b 0\r\n",
            )
        } else {
            (
                "convert",
                "#!/bin/sh\n\
                 echo \"Version: ImageMagick 6.9.11-60 Q16 x86_64 20200481 https://imagemagick.org\"\n",
            )
        };
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    /// Writes a script named `convert` whose version banner never mentions
    /// ImageMagick at all — standing in for some unrelated program that
    /// happens to share the name on `PATH`, which `magick_convert_fallback`
    /// must reject rather than adopt as the image backend.
    fn stub_convert_that_is_not_imagemagick(dir: &Path) -> PathBuf {
        let (name, body) = if cfg!(windows) {
            (
                "convert.bat",
                "@echo off\r\n\
                 echo convert (some unrelated tool) version 1.0\r\n\
                 exit /b 0\r\n",
            )
        } else {
            (
                "convert",
                "#!/bin/sh\n\
                 echo \"convert (some unrelated tool) version 1.0\"\n",
            )
        };
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    /// `--version` isn't a real ffmpeg (or ffprobe, or ImageMagick `magick`)
    /// option; all three take the single-dash `-version`.
    #[test]
    fn version_probe_uses_single_dash_for_ffmpeg_ffprobe_and_magick() {
        let dir = tempfile::tempdir().unwrap();
        let stub = stub_that_echoes_first_arg(dir.path());
        for backend in [Backend::Ffmpeg, Backend::Ffprobe, Backend::Magick] {
            let mut r = Resolver::new();
            r.with_override(backend, stub.clone());
            let resolved = r.resolve(backend).unwrap();
            assert_eq!(
                resolved.version, "-version-9",
                "{backend:?} must be probed with -version"
            );
        }
    }

    /// pandoc and soffice use the conventional double-dash `--version`.
    #[test]
    fn version_probe_uses_double_dash_for_pandoc_and_soffice() {
        let dir = tempfile::tempdir().unwrap();
        let stub = stub_that_echoes_first_arg(dir.path());
        for backend in [Backend::Pandoc, Backend::Soffice] {
            let mut r = Resolver::new();
            r.with_override(backend, stub.clone());
            let resolved = r.resolve(backend).unwrap();
            assert_eq!(
                resolved.version, "--version-9",
                "{backend:?} must be probed with --version"
            );
        }
    }

    /// A real ffmpeg build writes its `-version` banner to stderr, leaving
    /// stdout empty; `version_of` must fall back to stderr rather than
    /// reporting "unknown" for a backend that is actually installed.
    #[test]
    fn version_falls_back_to_stderr_when_stdout_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let stub = stub_that_prints_version_to_stderr_only(dir.path());
        let mut r = Resolver::new();
        r.with_override(Backend::Ffmpeg, stub);
        let resolved = r.resolve(Backend::Ffmpeg).unwrap();
        assert_eq!(resolved.version, "banner-9.0");
    }

    // --- Controller review round 5: extract a version token, not the
    // whole banner line, so doctor's tabular column can't be blown out ----

    #[test]
    fn extracts_the_version_token_from_a_real_ffmpeg_banner() {
        let line = "ffmpeg version 9.0-full_build-www.gyan.dev Copyright (c) 2000-2026 the FFmpeg developers";
        assert_eq!(
            extract_version_token(line),
            Some("9.0-full_build-www.gyan.dev")
        );
    }

    #[test]
    fn extracts_the_version_token_from_a_real_imagemagick_banner() {
        let line = "Version: ImageMagick 7.1.1-29 Q16-HDRI x64 20231001 https://imagemagick.org";
        assert_eq!(extract_version_token(line), Some("7.1.1-29"));
    }

    #[test]
    fn extracts_the_version_token_from_a_real_pandoc_banner() {
        assert_eq!(extract_version_token("pandoc 3.1.11"), Some("3.1.11"));
    }

    #[test]
    fn extracts_the_version_token_from_a_real_soffice_banner() {
        assert_eq!(
            extract_version_token("LibreOffice 7.6.4.1"),
            Some("7.6.4.1")
        );
    }

    /// No token on the line contains a digit at all: `version_of` must fall
    /// back to "unknown" rather than picking some arbitrary non-version
    /// word or panicking.
    #[test]
    fn no_digit_bearing_token_yields_none() {
        assert_eq!(
            extract_version_token("no digits anywhere on this line"),
            None
        );
    }

    // --- Windows soffice.exe incident: the version probe must never be
    // able to hang. `Command::output()`/`wait()` has no built-in timeout;
    // a misresolved `soffice.exe` (a GUI-subsystem launcher that pops its
    // own console window and never returns without a human to dismiss it —
    // see `well_known`'s docs) hung `doctor`, `resolve`, and `cargo test`
    // itself with no way to cancel. These stand in for that with a stub
    // that never exits on its own, proving `run_with_timeout` (and
    // `version_of`/`resolve` built on it) kills it and moves on instead.

    /// Writes a script that never exits by itself — an infinite loop —
    /// standing in for a genuinely hung backend without depending on any
    /// particular platform's GUI-subsystem quirks.
    fn stub_that_never_exits(dir: &Path) -> PathBuf {
        let (name, body) = if cfg!(windows) {
            ("never_exits.bat", "@echo off\r\n:loop\r\ngoto loop\r\n")
        } else {
            ("never_exits.sh", "#!/bin/sh\nwhile :; do :; done\n")
        };
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    /// A hung child must be killed at (not well after) its deadline, and
    /// must report `None` rather than a false success — proof at the
    /// mechanism level, beneath `version_of`.
    #[test]
    fn run_with_timeout_kills_a_hung_child_at_the_deadline_not_after() {
        let dir = tempfile::tempdir().unwrap();
        let stub = stub_that_never_exits(dir.path());
        let timeout = Duration::from_millis(150);

        let started = Instant::now();
        let out = Resolver::run_with_timeout(Command::new(&stub), timeout);
        let elapsed = started.elapsed();

        assert!(
            out.is_none(),
            "a hung child must report no output, not a false success"
        );
        assert!(
            elapsed >= timeout,
            "must not report a timeout before the deadline actually passed: {elapsed:?}"
        );
        assert!(
            elapsed < timeout * 10,
            "must be killed promptly at the deadline, not left running: {elapsed:?}"
        );
    }

    /// The complement: an ordinary, fast-exiting child still gets its
    /// output back — the timeout machinery must not interfere with the
    /// normal, non-hung case.
    #[test]
    fn run_with_timeout_returns_output_for_a_child_that_exits_promptly() {
        let dir = tempfile::tempdir().unwrap();
        let stub = stub_that_echoes_first_arg(dir.path());
        let mut cmd = Command::new(&stub);
        cmd.arg("--version");

        let out = Resolver::run_with_timeout(cmd, Duration::from_secs(5))
            .expect("a fast, well-behaved child must not be treated as hung");
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("--version-9"));
    }

    /// End-to-end through `resolve()`: a hung backend must resolve as
    /// present (the file genuinely exists) with version `"unknown"`,
    /// promptly, rather than hanging the whole call. `with_probe_timeout`
    /// shortens the real 5-second `VERSION_PROBE_TIMEOUT` so proving this
    /// doesn't cost the suite anywhere near that long.
    #[test]
    fn a_hung_version_probe_is_killed_and_reported_as_unknown_rather_than_hanging() {
        let dir = tempfile::tempdir().unwrap();
        let stub = stub_that_never_exits(dir.path());
        let mut r = Resolver::new();
        r.with_probe_timeout(Duration::from_millis(150));
        r.with_override(Backend::Ffmpeg, stub);

        let started = Instant::now();
        let resolved = r.resolve(Backend::Ffmpeg).unwrap();
        let elapsed = started.elapsed();

        assert_eq!(resolved.version, "unknown");
        assert!(
            elapsed < Duration::from_secs(3),
            "must not wait anywhere near the real 5s production timeout: {elapsed:?}"
        );
    }

    // --- Fix 2: ImageMagick 6's `convert` fallback for `Backend::Magick` --
    // Ubuntu/Debian's `apt-get install imagemagick` installs ImageMagick 6,
    // whose unified binary is `convert`, not `magick` (ImageMagick 7 only).
    // Without this fallback, `conv doctor`'s own advised install command
    // leaves convkit still unable to find ImageMagick.

    /// `magick_convert_fallback_applies` is the platform gate `resolve`
    /// consults before ever attempting the fallback. It must be `false` on
    /// Windows regardless of what's on `PATH` there — `convert.exe` on
    /// Windows is the OS's own destructive FAT->NTFS conversion tool, not
    /// ImageMagick, and is the reason ImageMagick renamed its unified
    /// binary. Asserted directly on the predicate (not through `resolve`)
    /// so this is deterministic on every host in the CI matrix, including
    /// windows-latest, where `cfg!(windows)` is always `true` and a
    /// `resolve`-based test could never itself prove the `false` branch
    /// still behaves correctly.
    #[test]
    fn convert_fallback_never_applies_on_windows() {
        assert!(!Resolver::magick_convert_fallback_applies(
            Backend::Magick,
            true
        ));
    }

    /// The mirror image: off Windows, the fallback does apply to `Magick`.
    #[test]
    fn convert_fallback_applies_to_magick_off_windows() {
        assert!(Resolver::magick_convert_fallback_applies(
            Backend::Magick,
            false
        ));
    }

    /// The fallback is specific to `Backend::Magick` — ffmpeg, ffprobe,
    /// pandoc, and soffice have no `convert`-shaped equivalent and must
    /// never attempt this path, on any platform.
    #[test]
    fn convert_fallback_never_applies_to_other_backends() {
        for backend in [
            Backend::Ffmpeg,
            Backend::Ffprobe,
            Backend::Pandoc,
            Backend::Soffice,
        ] {
            assert!(!Resolver::magick_convert_fallback_applies(backend, false));
        }
    }

    /// A `convert` whose `-version` banner genuinely says ImageMagick is
    /// accepted as the `Magick` backend, resolved via `Source::Path`.
    #[test]
    fn a_real_imagemagick_convert_is_accepted_by_the_fallback() {
        let dir = tempfile::tempdir().unwrap();
        stub_convert_that_is_really_imagemagick(dir.path());
        let mut r = Resolver::new();
        r.with_convert_search_dir(dir.path().to_path_buf());

        let resolved = r
            .magick_convert_fallback()
            .expect("a real ImageMagick convert must be accepted");
        assert_eq!(resolved.backend, Backend::Magick);
        assert_eq!(resolved.source, Source::Path);
        assert_eq!(resolved.version, "6.9.11-60");
    }

    /// A `convert` whose version output does not mention ImageMagick at all
    /// — some unrelated program that merely shares the name — must be
    /// rejected outright, not adopted as the image backend.
    #[test]
    fn a_convert_that_is_not_imagemagick_is_rejected_by_the_fallback() {
        let dir = tempfile::tempdir().unwrap();
        stub_convert_that_is_not_imagemagick(dir.path());
        let mut r = Resolver::new();
        r.with_convert_search_dir(dir.path().to_path_buf());

        assert!(
            r.magick_convert_fallback().is_none(),
            "a convert that never mentions ImageMagick must not be accepted"
        );
    }

    /// No `convert` anywhere in the (test-controlled) search directory: the
    /// fallback must report nothing rather than panicking or picking up
    /// some other file.
    #[test]
    fn no_convert_anywhere_yields_no_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = Resolver::new();
        r.with_convert_search_dir(dir.path().to_path_buf());

        assert!(r.magick_convert_fallback().is_none());
    }

    // --- Task 2: AvailableBackends / check_availability --------------------

    #[test]
    fn available_backends_collects_from_an_iterator_of_backends() {
        let available: AvailableBackends = [Backend::Pandoc, Backend::Typst].into_iter().collect();
        assert!(available.has(Backend::Pandoc));
        assert!(available.has(Backend::Typst));
        assert!(!available.has(Backend::Soffice));
    }

    #[test]
    fn available_backends_default_is_empty() {
        let available = AvailableBackends::default();
        for b in [
            Backend::Ffmpeg,
            Backend::Ffprobe,
            Backend::Magick,
            Backend::Soffice,
            Backend::Pandoc,
            Backend::Typst,
        ] {
            assert!(!available.has(b));
        }
    }

    /// This used to need `without_well_known` plus a best-effort conditional
    /// on `resolve()` agreeing Soffice was missing, because a bogus
    /// `--soffice-path`-equivalent override fell through to `Env`/`Path`/
    /// `WellKnown` -- any of which could genuinely resolve on a host with a
    /// real LibreOffice install (this project's own dev machine included).
    /// Since the override-authority fix, that fall-through no longer
    /// happens at all: a `Source::Override` path that doesn't exist is now
    /// a hard, immediate `InvalidInvocation` error (see `Resolver::resolve`'s
    /// docs), so `check_availability`'s `.is_ok()` filter marks Soffice
    /// unavailable deterministically, on every host, with no
    /// `without_well_known` needed. `Pandoc`'s override still points at a
    /// real, existing stub file, so `resolve()` returns it from the very
    /// first candidate it tries.
    #[test]
    fn check_availability_only_marks_backends_that_actually_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let stub = stub_that_echoes_first_arg(dir.path());
        let mut r = Resolver::new();
        r.with_override(Backend::Pandoc, stub);
        r.with_override(Backend::Soffice, PathBuf::from("/definitely/not/here"));

        let available = r.check_availability(&[Backend::Pandoc, Backend::Soffice]);
        assert!(available.has(Backend::Pandoc));
        assert!(!available.has(Backend::Soffice));
    }

    #[test]
    fn check_availability_never_probes_a_backend_outside_the_given_candidates() {
        // Soffice is never passed in, so even though it would fail to
        // resolve anyway here, this proves the method is scoped to exactly
        // its `candidates` argument rather than some fixed internal list.
        let dir = tempfile::tempdir().unwrap();
        let stub = stub_that_echoes_first_arg(dir.path());
        let mut r = Resolver::new();
        r.with_override(Backend::Pandoc, stub);

        let available = r.check_availability(&[Backend::Pandoc]);
        assert!(available.has(Backend::Pandoc));
        assert!(!available.has(Backend::Soffice));
        assert!(!available.has(Backend::Typst));
    }

    /// When a real `magick` is found anywhere in the ordinary candidate
    /// chain (here: via an override, standing in for any of
    /// override/env/managed/path), `resolve` must return it and must never
    /// fall through to the `convert` fallback — even when a
    /// perfectly-valid-looking ImageMagick `convert` also exists. `magick`
    /// always wins.
    #[test]
    fn magick_takes_precedence_over_the_convert_fallback_when_both_exist() {
        let magick_dir = tempfile::tempdir().unwrap();
        let magick_stub = stub_that_echoes_first_arg(magick_dir.path());
        let convert_dir = tempfile::tempdir().unwrap();
        stub_convert_that_is_really_imagemagick(convert_dir.path());

        let mut r = Resolver::new();
        r.with_override(Backend::Magick, magick_stub.clone());
        r.with_convert_search_dir(convert_dir.path().to_path_buf());

        let resolved = r.resolve(Backend::Magick).unwrap();
        assert_eq!(resolved.path, magick_stub);
        assert_eq!(resolved.source, Source::Override);
    }

    // --- overrides_only: closes the whole candidate chain but Override ----

    /// The basic shape: with no override set for a backend at all,
    /// `overrides_only` must make `candidates()` return nothing for it --
    /// not fall through to `Env`/`Managed`/`Path`/`WellKnown`.
    #[test]
    fn overrides_only_yields_no_candidates_for_a_backend_with_no_override() {
        let mut r = Resolver::new();
        r.overrides_only();
        assert_eq!(r.candidates(Backend::Soffice), Vec::new());
    }

    /// The complement: with an override set, `overrides_only` still yields
    /// exactly that one `Source::Override` candidate and nothing else.
    #[test]
    fn overrides_only_yields_exactly_the_override_candidate_when_one_is_set() {
        let mut r = Resolver::new();
        r.overrides_only();
        let fake = PathBuf::from("/nowhere/custom-pandoc");
        r.with_override(Backend::Pandoc, fake.clone());
        assert_eq!(
            r.candidates(Backend::Pandoc),
            vec![(fake, Source::Override)]
        );
    }

    /// The interaction the fix brief calls out by name: `overrides_only`
    /// and `Source::Env` must not both apply. It is not enough for
    /// `overrides_only` to happen to work when `CONVKIT_SOFFICE` is unset in
    /// whatever environment the suite happens to run in -- a developer (or,
    /// per the fix brief, this project's own dev machine) with
    /// `CONVKIT_SOFFICE` genuinely exported must see it ignored too, or
    /// `overrides_only` silently stops doing its job on exactly the
    /// machines it exists to guard against.
    ///
    /// Proving this means a real `CONVKIT_SOFFICE` must be set while
    /// `candidates()` runs. Mutating this process's own environment
    /// directly would race every other test thread reading the same
    /// variable -- precisely the hazard `without_well_known`'s and
    /// `libreoffice_is_never_offered_as_a_managed_install`'s docs above
    /// describe, and precisely why `cli.rs`'s own env-var-sensitive tests
    /// use `assert_cmd` to spawn a genuinely separate child process instead
    /// of mutating `std::env` in-process. There is no `assert_cmd` here --
    /// this is a `convkit-core` unit test, not a `conv` integration test --
    /// so this re-execs the compiled test binary itself as that separate
    /// child process, the same isolation technique applied one level down:
    /// `CONVKIT_SOFFICE` is set only in the child's own environment block,
    /// never touching this (the parent) process's real environment, so no
    /// sibling test running concurrently in this process can ever observe
    /// it.
    #[test]
    fn overrides_only_ignores_the_convkit_env_var_even_with_no_override_set() {
        const CHILD_MARKER: &str = "CONVKIT_TEST_OVERRIDES_ONLY_ENV_CHECK_CHILD";

        if std::env::var_os(CHILD_MARKER).is_some() {
            // Running as the re-exec'd child, with a real CONVKIT_SOFFICE
            // set in *this* process's environment: `overrides_only` must
            // still yield no candidates for Soffice, since no override was
            // ever set for it.
            let mut r = Resolver::new();
            r.overrides_only();
            let candidates = r.candidates(Backend::Soffice);
            assert!(
                candidates.is_empty(),
                "overrides_only must ignore CONVKIT_SOFFICE when no override \
                 is set for the backend: {candidates:?}"
            );
            return;
        }

        let exe = std::env::current_exe().expect("test binary must have a path");
        let output = Command::new(&exe)
            .arg("resolve::tests::overrides_only_ignores_the_convkit_env_var_even_with_no_override_set")
            .arg("--exact")
            .arg("--nocapture")
            .env(CHILD_MARKER, "1")
            .env("CONVKIT_SOFFICE", "/definitely/not/here/soffice")
            .output()
            .expect("failed to re-exec the test binary");

        assert!(
            output.status.success(),
            "child process (real CONVKIT_SOFFICE set) failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
