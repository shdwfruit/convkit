"""Simulate a non-Windows build's view of the test modules, on Windows.

The recurring failure: a test helper used only by `#[cfg(windows)]` tests is
dead code on Linux and macOS, where `-D warnings` turns that into a build
failure. It cannot be seen locally because cross-compiling this workspace
needs a C toolchain for `ring`.

This rewrites `windows` to a cfg name that is never set, in cfg *attributes*
appearing after each file's `#[cfg(test)]` marker -- and only there, so
production cfg blocks that genuinely need the Windows APIs are untouched.
`#[cfg(windows)]` items then compile out and `#[cfg(not(windows))]` items
compile in, which is exactly a Linux build's view, so dead-code analysis sees
what Linux sees.

Usage, from the repo root:

    python scripts/flip-platform-cfgs.py . apply
    cargo clippy --workspace --all-targets -- -D warnings -A unexpected_cfgs
    python scripts/flip-platform-cfgs.py . revert

It edits files in place; `revert` is idempotent, and `git status` shows any
residue if a run is interrupted.
"""
import io
import os
import sys

REPO = sys.argv[1]
MODE = sys.argv[2]

MARKERS = ('#[cfg(test)]',)
SWAPS = [
    ('#[cfg(windows)]', '#[cfg(FLIPPED_unix)]'),
    ('#[cfg(not(windows))]', '#[cfg(not(FLIPPED_unix))]'),
]
# `FLIPPED_unix` is not a real cfg, so it is always false and its negation
# always true -- the same shape as `windows` on Linux, without depending on
# how the host defines `unix`.

changed = 0
for root, _dirs, files in os.walk(os.path.join(REPO, 'crates')):
    if 'target' in root:
        continue
    for name in files:
        if not name.endswith('.rs'):
            continue
        path = os.path.join(root, name)
        text = io.open(path, encoding='utf-8').read()
        idx = -1
        for marker in MARKERS:
            found = text.find(marker)
            if found >= 0 and (idx < 0 or found < idx):
                idx = found
        if idx < 0:
            continue
        head, tail = text[:idx], text[idx:]
        before = tail
        for old, new in SWAPS:
            tail = tail.replace(new, old) if MODE == 'revert' else tail.replace(old, new)
        if tail != before:
            io.open(path, 'w', encoding='utf-8', newline='\n').write(head + tail)
            changed += 1
print(f'{MODE}: {changed} files')
