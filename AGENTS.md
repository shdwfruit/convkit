# AGENTS.md

Instructions for coding agents working **on** convkit. To *use* `conv` from
an agent, see [skills/convkit/SKILL.md](skills/convkit/SKILL.md).

## Layout

- `crates/convkit-core`: the library. Formats (`format.rs`), one recipe per
  pair (`registry.rs`), planning and running conversions. **It never prints
  to stdout or stderr**; CI fails if it does. It returns typed values, which
  is what keeps `--json` complete and lets other frontends reuse it.
- `crates/conv`: the binary. Argument parsing, prompts and all output,
  human and `--json`.
- `crates/convkit-diff`: a differential test harness that backs the
  quality claims and gates swapping one backend for another.
- `README.md` and the code are authoritative. `docs/design.md` is
  historical. `docs/local/` holds unpublished design notes and is gitignored.

## Build and test

```console
cargo build
./target/debug/conv clip.mp4 clip.gif --dry-run   # no backends needed
cargo test --workspace                             # no backends needed
cargo test --workspace -- --ignored                # real ffmpeg/magick/soffice/pandoc
```

Before you call a change done, run what CI runs:

```console
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The main test is a snapshot of every pair's exact backend argv in
`crates/convkit-core/tests/snapshots/`. Changing a recipe fails it on
purpose. Read the diff, and accept it with `cargo insta review` only if
every changed line is one you meant. Output checks that need real backends
(codec, orientation, page count) go in
`crates/convkit-core/tests/output_properties.rs`.

## Adding a conversion pair

1. Write the recipe in `registry.rs` in its family's `insert_*_family`
   function, using a neighbouring recipe as the template.
2. For a new format, add it to the `Format` enum and `TABLE` in `format.rs`.
3. Accept the new snapshot lines.
4. Update the pair and format counts at the top of the README
   (`conv capabilities` has the real numbers).

Comment every non-obvious flag with *why* it's there. The defaults are the
product.

## Rules that must hold

- **Offline.** Only `conv install`, `conv update` and `conv link --follow`
  may touch the network. Nothing else may open a socket, and no file
  content is ever uploaded.
- **`--json`.** Every command emits exactly one JSON document: `"ok"`
  plus one plural key. Failures carry a stable snake_case `error.code`
  (`ErrorCode` in `crates/convkit-core/src/error.rs`) and a remediation
  when one exists. Under `--json`, conv never prompts and never installs.
  Don't add, rename or remove error codes or JSON fields casually, since
  scripts and agents depend on them.
- **Behaviour.** A flag that doesn't apply to a pair is an error, not
  silently ignored. A lossy or surprising result gets a note. Something
  drastic needs an explicit flag or a `[y/N]` (`--yes` for scripts).
  Existing files are never overwritten without `-y`. Every failure says how
  to fix it.
- **Secrets.** Passwords are never accepted as command-line arguments (use
  a prompt or `CONVKIT_PASSWORD`). `conv wifi` prints a password only to a
  terminal, never into a pipe or `--json`.
- **Docs.** A user-visible change updates the README and `CHANGELOG.md` in
  the same plain style: short sentences, concrete examples, real output.

## Commits and PRs

One change per PR. Commit messages look like `feat(video): ...` or
`fix(resize): ...`, with a body that says why. The PR description says how
it was tested.
