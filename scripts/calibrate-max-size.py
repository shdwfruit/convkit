#!/usr/bin/env python3
"""Measure what the --max-size cost model needs, and fit its constants.

Encodes excerpts of Xiph's uncompressed test clips across a grid of
resolutions and bits per pixel with the same two-pass invocation convkit
uses, scores each against its source with VMAF, and prints:

  * the fitted VideoFit constants per codec (X264_FIT / VP9_FIT in
    crates/convkit-core/src/budget.rs)
  * OVERHEAD_PERMILLE and MARGIN_PERMILLE from the measured size error

Needs ffmpeg built with libvmaf, libx264 and libvpx-vp9. Standard library
only. Re-running skips work already done in --work, so an interrupted run
resumes. --verbose prints every ffmpeg command, so any one can be rerun by
hand. The --work path must not contain characters that mean something
inside a filter graph (':' ',' ';' quotes, brackets, backslash), because it
appears there.

Each clip is downloaded once and checksummed, turned into an exact 1080p
y4m reference, and the download is then deleted to save disk. Every encode
is scored against the reference, and results.csv keeps every measurement.

Usage:
  scripts/calibrate-max-size.py --work DIR [--codecs x264,vp9]
      [--frames 250] [--vp9-clips in_to_tree] [--threads N] [--verbose]
"""

import argparse
import csv
import glob
import hashlib
import json
import math
import os
import statistics
import subprocess
import sys
import time
import urllib.request

# Xiph's "derf" collection: 1080p, 50 fps, 4:2:0, uncompressed y4m. Only
# the first --frames frames are fetched (an HTTP range), so the bytes, and
# therefore the checksums printed, are reproducible.
CLIPS = {
    "crowd_run": "https://media.xiph.org/video/derf/y4m/crowd_run_1080p50.y4m",
    "in_to_tree": "https://media.xiph.org/video/derf/y4m/in_to_tree_1080p50.y4m",
    "old_town_cross": "https://media.xiph.org/video/derf/y4m/old_town_cross_1080p50.y4m",
}
SRC_W, SRC_H, SRC_FPS = 1920, 1080, 50
FRAME_BYTES = SRC_W * SRC_H * 3 // 2 + len(b"FRAME\n")
HEADER_ALLOWANCE = 4096
SHORT_SIDES = [1080, 720, 540, 360, 240]
BPPS = [0.005, 0.01, 0.02, 0.04, 0.08, 0.16]

# The search ranges of the fit, (low, high, step) per VideoFit field. A
# result on the edge of its range is reported: the true optimum may lie
# beyond it.
GRID = {
    "res_scale": (4.0, 20.0, 1.0),
    "res_power": (1.0, 3.0, 0.25),
    "bpp_half": (0.002, 0.030, 0.001),
    "bpp_slope": (0.8, 2.2, 0.1),
}

VERBOSE = False
FILTER_GRAPH_SPECIALS = ":,;'[]\\"


def run(cmd):
    if VERBOSE:
        print("+", " ".join(cmd), file=sys.stderr)
    done = subprocess.run(cmd, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                          stderr=subprocess.PIPE, text=True)
    if done.returncode != 0:
        sys.exit(f"command failed ({done.returncode}): {' '.join(cmd)}\n{done.stderr}")


def sha256_of(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while chunk := f.read(1 << 20):
            h.update(chunk)
    return h.hexdigest()


def range_end(frames):
    return HEADER_ALLOWANCE + (frames + 1) * FRAME_BYTES


def fetch(name, url, work, frames):
    """The clip's exact y4m reference and the SHA-256 of the bytes it came from."""
    base = os.path.join(work, name)
    part, ref, digest_file = base + ".part.y4m", base + ".ref.y4m", base + ".sha256"
    if os.path.exists(ref) and os.path.exists(digest_file):
        with open(digest_file) as f:
            return ref, f.read().strip()
    if not os.path.exists(part):
        req = urllib.request.Request(url, headers={"Range": f"bytes=0-{range_end(frames)}"})
        with urllib.request.urlopen(req) as r:
            if r.status != 206:
                sys.exit(f"{url} ignored the byte range (HTTP {r.status}); refusing to "
                         f"download the whole file")
            with open(part + ".tmp", "wb") as f:
                while chunk := r.read(1 << 20):
                    f.write(chunk)
        os.replace(part + ".tmp", part)
    digest = sha256_of(part)
    run(["ffmpeg", "-y", "-v", "error", "-i", part, "-frames:v", str(frames),
         "-pix_fmt", "yuv420p", "-f", "yuv4mpegpipe", ref + ".tmp"])
    excess = os.path.getsize(ref + ".tmp") - frames * FRAME_BYTES
    if not 0 < excess < HEADER_ALLOWANCE:
        sys.exit(f"{name}: the reference holds the wrong number of frames "
                 f"(size is {excess} bytes off {frames} frames); was the download cut short?")
    os.replace(ref + ".tmp", ref)
    with open(digest_file, "w") as f:
        f.write(digest + "\n")
    os.remove(part)
    return ref, digest


def even(short):
    """Width for this short side, rounded to the nearest even value as
    resolution_steps in budget.rs does."""
    return (short * SRC_W + SRC_H) // (2 * SRC_H) * 2


def encode(ref, out, codec, w, h, bps, passlog, frames):
    # What convkit emits: no resize filter at the source's own size, then the
    # even-dimension guard every libx264 transcode carries.
    chain = "scale=trunc(iw/2)*2:trunc(ih/2)*2"
    if (w, h) != (SRC_W, SRC_H):
        chain = f"scale=w={w}:h={h},{chain}"
    if codec == "x264":
        enc = ["-c:v", "libx264", "-b:v", str(bps)]
        companions = ["-pix_fmt", "yuv420p"]
        tail = ["-movflags", "+faststart"]
    else:
        enc = ["-c:v", "libvpx-vp9", "-b:v", str(bps)]
        companions = ["-row-mt", "1", "-threads", "0"]
        tail = []
    common = ["ffmpeg", "-y", "-v", "error", "-i", ref, "-frames:v", str(frames),
              "-map", "0:v:0", "-vf", chain, *enc]
    root, ext = os.path.splitext(out)
    partial = root + ".partial" + ext
    try:
        run([*common, "-pass", "1", "-passlogfile", passlog, *companions,
             "-an", "-sn", "-dn", "-f", "null", "-"])
        run([*common, "-pass", "2", "-passlogfile", passlog, *companions,
             "-an", *tail, partial])
        os.replace(partial, out)
    finally:
        for leftover in glob.glob(glob.escape(passlog) + "*"):
            os.remove(leftover)


def vmaf(dist, ref, threads):
    log = dist + ".vmaf.json"
    if not os.path.exists(log):
        graph = (f"[0:v]scale={SRC_W}:{SRC_H}:flags=bicubic,setpts=PTS-STARTPTS[d];"
                 f"[1:v]setpts=PTS-STARTPTS[r];"
                 f"[d][r]libvmaf=log_fmt=json:log_path={log}.partial:n_threads={threads}")
        run(["ffmpeg", "-v", "error", "-i", dist, "-i", ref, "-lavfi", graph,
             "-f", "null", "-"])
        os.replace(log + ".partial", log)
    with open(log) as f:
        return json.load(f)["pooled_metrics"]["vmaf"]["mean"]


def predict(fit, scale, bpp):
    """Predicted VMAF: 100 minus budget.rs's video_loss."""
    res_scale, res_power, bpp_half, bpp_slope = fit
    octaves = 0.0 if scale >= 1 else math.log2(1 / scale)
    res_loss = min(100.0, res_scale * octaves ** res_power)
    artefact = 100.0 / (1.0 + (max(bpp, 1e-9) / bpp_half) ** bpp_slope)
    return (100 - res_loss) * (1 - artefact / 100)


def frange(lo, hi, step):
    n = int(round((hi - lo) / step))
    return [round(lo + i * step, 6) for i in range(n + 1)]


def fit(rows):
    best = None
    for rs in frange(*GRID["res_scale"]):
        for rp in frange(*GRID["res_power"]):
            for bh in frange(*GRID["bpp_half"]):
                for bs in frange(*GRID["bpp_slope"]):
                    f = (rs, rp, bh, bs)
                    err = sum((predict(f, r["scale"], r["bpp"]) - r["vmaf"]) ** 2 for r in rows)
                    if best is None or err < best[0]:
                        best = (err, f)
    err, f = best
    return f, math.sqrt(err / len(rows))


def main():
    global VERBOSE
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True)
    ap.add_argument("--codecs", default="x264,vp9")
    ap.add_argument("--frames", type=int, default=250)
    ap.add_argument("--vp9-clips", default="in_to_tree",
                    help="VP9 is slow; calibrate it on these clips only")
    ap.add_argument("--threads", type=int, default=os.cpu_count() or 4)
    ap.add_argument("--verbose", action="store_true")
    args = ap.parse_args()
    VERBOSE = args.verbose
    work = os.path.abspath(args.work)
    if any(c in work for c in FILTER_GRAPH_SPECIALS):
        sys.exit(f"--work must not contain any of {FILTER_GRAPH_SPECIALS}")
    os.makedirs(work, exist_ok=True)
    # Encodes are named without their length, so a directory holds one length.
    marker = os.path.join(work, "frames")
    if os.path.exists(marker):
        with open(marker) as f:
            held = int(f.read())
        if held != args.frames:
            sys.exit(f"{work} was made with --frames {held}; use another --work")
    else:
        with open(marker, "w") as f:
            f.write(f"{args.frames}\n")
    seconds = args.frames / SRC_FPS
    started = time.monotonic()

    refs = {}
    for name, url in CLIPS.items():
        refs[name], digest = fetch(name, url, work, args.frames)
        print(f"clip {name}: {url} bytes 0-{range_end(args.frames)} sha256 {digest}", flush=True)

    rows = []
    for codec in args.codecs.split(","):
        clips = CLIPS if codec == "x264" else [c for c in CLIPS if c in args.vp9_clips.split(",")]
        ext = "mp4" if codec == "x264" else "webm"
        for name in clips:
            for short in SHORT_SIDES:
                w, h = even(short), short
                for bpp in BPPS:
                    began = time.monotonic()
                    bps = int(bpp * w * h * SRC_FPS)
                    out = os.path.join(work, f"{name}.{codec}.{short}.{bpp}.{ext}")
                    if not os.path.exists(out):
                        encode(refs[name], out, codec, w, h, bps,
                               out + ".pass", args.frames)
                    size = os.path.getsize(out)
                    actual_bpp = size * 8 / seconds / (w * h * SRC_FPS)
                    rows.append({
                        "clip": name, "codec": codec, "short": short, "w": w, "h": h,
                        "target_bpp": bpp, "bps": bps, "bytes": size,
                        "size_ratio": size / (bps * seconds / 8),
                        "scale": short / SRC_H, "bpp": actual_bpp,
                        "vmaf": vmaf(out, refs[name], args.threads),
                    })
                    print(f"{name} {codec} {w}x{h} bpp {bpp}: "
                          f"{rows[-1]['vmaf']:.2f} vmaf, ratio {rows[-1]['size_ratio']:.4f}, "
                          f"{time.monotonic() - began:.1f} s", flush=True)

    with open(os.path.join(work, "results.csv"), "w", newline="") as f:
        wr = csv.DictWriter(f, fieldnames=list(rows[0].keys()))
        wr.writeheader()
        wr.writerows(rows)

    print("\n// --- paste into crates/convkit-core/src/budget.rs ---")
    for codec, const in (("x264", "X264_FIT"), ("vp9", "VP9_FIT")):
        sub = [r for r in rows if r["codec"] == codec]
        if not sub:
            continue
        best, rmse = fit(sub)
        rs, rp, bh, bs = best
        print(f"pub const {const}: VideoFit = VideoFit {{ res_scale: {rs:.1f}, "
              f"res_power: {rp:.2f}, bpp_half: {bh:.3f}, bpp_slope: {bs:.1f} }}; "
              f"// rmse {rmse:.2f} VMAF over {len(sub)} encodes")
        for field, value in zip(GRID, best):
            lo, hi, _ = GRID[field]
            if value in (lo, hi):
                print(f"// note: {const} {field} is at the edge of its search range "
                      f"({lo}-{hi}); the optimum may lie beyond it")
    ratios = sorted(r["size_ratio"] for r in rows)
    median = statistics.median(ratios)
    p90 = ratios[min(len(ratios) - 1, int(0.9 * len(ratios)))]
    overhead = max(0, math.ceil((median - 1) * 1000))
    margin = max(10, math.ceil((p90 - median) * 1000))
    print(f"pub const OVERHEAD_PERMILLE: u64 = {overhead}; // median size ratio {median:.4f}")
    print(f"pub const MARGIN_PERMILLE: u64 = {margin}; // p90 size ratio {p90:.4f}")
    print(f"\n// finished in {(time.monotonic() - started) / 60:.1f} minutes")


if __name__ == "__main__":
    main()
