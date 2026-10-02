# Contributing

Thanks for taking a look. Issues labelled
[good first issue](https://github.com/shdwfruit/convkit/labels/good%20first%20issue)
are scoped so you can pick one up without knowing the whole codebase.
Comment on the issue before you start so two people don't do the same work.
Questions go in [Discussions](https://github.com/shdwfruit/convkit/discussions).

## Building

You need Rust 1.85+ and a C toolchain (the `ring` dependency compiles C).

```console
$ cargo build
$ ./target/debug/conv clip.mp4 clip.gif --dry-run
```

`--dry-run` prints the backend command without running it, so you can work
on most of conv without ffmpeg, ImageMagick, LibreOffice or pandoc installed.

## Tests

```console
$ cargo test --workspace                 # no backends needed
$ cargo test --workspace -- --ignored    # runs the real backends
```

The first runs everywhere with nothing installed. Its main check is a
snapshot of the exact argv every conversion pair hands its backend, in
`crates/convkit-core/tests/snapshots/`. If you change a recipe, that
snapshot fails on purpose: read the diff, and if it's what you meant, accept
it with `cargo insta review` (`cargo install cargo-insta`).

The `--ignored` tests run real conversions and check the output files. They
need ffmpeg, ImageMagick, LibreOffice and pandoc; `conv doctor` shows what
you have. CI runs them on Ubuntu, so it's fine to leave them to CI.

Before opening a PR, run what CI runs:

```console
$ cargo fmt --all
$ cargo clippy --workspace --all-targets -- -D warnings
$ cargo test --workspace
```

## Layout

- `crates/convkit-core`: the library. Formats (`format.rs`), the recipe
  for each pair (`registry.rs`), planning and running conversions. It never
  prints; CI fails if it does.
- `crates/conv`: the `conv` binary. Argument parsing, prompts and all
  output.

## Adding a conversion pair

1. Write the recipe in `crates/convkit-core/src/registry.rs` and insert it
   in its family's `insert_*_family` function. Existing recipes in the same
   family are the best template.
2. If the format is new, add it to the `Format` enum and the `TABLE` in
   `format.rs`.
3. Run `cargo test --workspace` and accept the new snapshot lines.
4. If the output needs checking for real (orientation, codecs, page
   count), add a test to `crates/convkit-core/tests/output_properties.rs`.
5. Update the pair and format counts at the top of the README
   (`conv capabilities` has the real numbers).

Explain any non-obvious flag in a comment. Most of the value in this
project is in the defaults, and the next person needs to know why each
one is there.

## How conv should behave

Let people do what they ask, but make sure they know what they're getting.
A flag that doesn't apply to a pair is an error, not silently ignored. A
lossy or surprising result gets a note. Something drastic needs an explicit
flag or a `[y/N]`. A failure always says how to fix it.

## Pull requests

Keep a PR to one change. Commit messages follow the existing style, e.g.
`feat(video): ...` or `fix(resize): ...`, with a body that says why. The
PR description should say how you tested it.

By contributing you agree your work is licensed under the same terms as the
project, MIT or Apache-2.0 at the user's choice.
