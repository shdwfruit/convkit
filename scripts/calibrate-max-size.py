#!/usr/bin/env python3
"""Measure what the --max-size cost model needs, and fit its constants.

Encodes excerpts of Xiph's uncompressed test clips across a grid of
resolutions and bits per pixel with the same two-pass invocation convkit
uses, scores each against its source with VMAF, and prints:

  * the fitted VideoFit constants per codec (X264_FIT / VP9_FIT in
    crates/convkit-core/src/budget.rs)
  * OVERHEAD_PERMILLE and MARGIN_PERMILLE from the measured size error

The size error is taken per codec, from the budget's operating region only:
encodes of at least 256 KiB (on a file of a few kilobytes the first frame and
the container are most of the bytes, which says nothing about the
multi-megabyte files the budget is used for) whose target is at least
--reserve-min-bpp bits per pixel (0.04 by default). Below that the overshoot
is an encoder saturating, not a bias to reserve for: libx264 cannot spend
fewer bits than its floor on a picture this starved, and the executor
re-plans at a smaller picture when an encode comes back over. The reserve is
the larger codec's 90th percentile over target; the overhead is that codec's
median, and the margin is what remains.

Needs ffmpeg built with libvmaf, libx264 and libvpx-vp9 (checked before
anything is downloaded). Standard library only. Re-running skips work
already done in --work, so an interrupted run resumes. --verbose prints
every ffmpeg command, quoted for a shell, so any one can be rerun by hand.
The --work path must not contain characters that mean something inside a
filter graph (':' ',' ';' quotes, brackets, backslash), because it appears
there.

Each clip is downloaded once and checksummed, turned into an exact 1080p
y4m reference, and the download is then deleted to save disk. Every encode
is scored against the reference. results-x264.csv and results-vp9.csv keep
every measurement, one file per codec, so running one codec never
overwrites the other's.

Usage:
  scripts/calibrate-max-size.py --work DIR [--codecs x264,vp9]
      [--frames 250] [--vp9-clips in_to_tree] [--reserve-min-bpp 0.04]
      [--threads N] [--verbose]
"""

import argparse
import csv
import glob
import hashlib
import http.client
import json
import math
import os
import re
import shlex
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
CODECS = {"x264": "libx264", "vp9": "libvpx-vp9"}
SRC_W, SRC_H, SRC_FPS = 1920, 1080, 50
FRAME_BYTES = SRC_W * SRC_H * 3 // 2 + len(b"FRAME\n")
HEADER_ALLOWANCE = 4096
SHORT_SIDES = [1080, 720, 540, 360, 240]
BPPS = [0.005, 0.01, 0.02, 0.04, 0.08, 0.16]
# Seconds a socket may sit silent before a download is given up on.
READ_TIMEOUT = 120
# Smaller encodes are left out of the size statistics.
MIN_STAT_BYTES = 256 * 1024
# So are encodes aimed below this many bits per pixel, unless --reserve-min-bpp
# says otherwise.
DEFAULT_RESERVE_MIN_BPP = 0.04
# The reserve is never thinner than this fraction of the target.
MIN_RESERVE = 0.01


def linear(lo, hi, step):
    return [round(lo + i * step, 6) for i in range(int(round((hi - lo) / step)) + 1)]


def geometric(lo, hi, count):
    return [round(lo * (hi / lo) ** (i / (count - 1)), 5) for i in range(count)]


# The values the fit searches, per VideoFit field. bpp_half spans a factor
# of thirty, so it is searched in equal ratios rather than equal steps. A
# result on either end of its range is reported: the true optimum may lie
# beyond it.
GRID = {
    "res_scale": linear(4.0, 40.0, 1.0),
    "res_power": linear(1.0, 3.0, 0.25),
    "bpp_half": geometric(0.002, 0.06, 60),
    "bpp_slope": linear(0.4, 2.2, 0.1),
}

VERBOSE = False
FILTER_GRAPH_SPECIALS = ":,;'[]\\"


def run(cmd):
    if VERBOSE:
        print("+", shlex.join(cmd), file=sys.stderr)
    done = subprocess.run(cmd, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                          stderr=subprocess.PIPE, text=True)
    if done.returncode != 0:
        sys.exit(f"command failed ({done.returncode}): {shlex.join(cmd)}\n{done.stderr}")


def ffmpeg_output(*args):
    try:
        done = subprocess.run(["ffmpeg", "-hide_banner", *args], stdin=subprocess.DEVNULL,
                              capture_output=True, text=True)
    except FileNotFoundError:
        sys.exit("ffmpeg is not on PATH; install one built with libvmaf, libx264 and libvpx-vp9")
    return done.stdout


def preflight(codecs):
    """Refuse to start without what the run needs; return ffmpeg's version line."""
    filters = ffmpeg_output("-filters")
    if not re.search(r"\blibvmaf\b", filters):
        sys.exit("this ffmpeg has no libvmaf filter; use a build configured with "
                 "--enable-libvmaf (no other metric is substituted)")
    encoders = ffmpeg_output("-encoders")
    for codec in codecs:
        if not re.search(rf"\b{re.escape(CODECS[codec])}\b", encoders):
            sys.exit(f"this ffmpeg has no {CODECS[codec]} encoder, which --codecs {codec} "
                     f"needs; use a build that includes it")
    lines = ffmpeg_output("-version").splitlines()
    return lines[0] if lines else "ffmpeg (version unknown)"


def sha256_of(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while chunk := f.read(1 << 20):
            h.update(chunk)
    return h.hexdigest()


def range_end(frames):
    return HEADER_ALLOWANCE + (frames + 1) * FRAME_BYTES


def discard(*paths):
    for path in paths:
        if os.path.exists(path):
            os.remove(path)


def download(url, part, frames):
    """Fetch the first bytes of `url` into `part`, or exit with nothing left behind."""
    wanted = range_end(frames) + 1
    tmp = part + ".tmp"
    req = urllib.request.Request(url, headers={"Range": f"bytes=0-{wanted - 1}"})
    try:
        with urllib.request.urlopen(req, timeout=READ_TIMEOUT) as r:
            if r.status != 206:
                sys.exit(f"{url} ignored the byte range (HTTP {r.status}); refusing to "
                         f"download the whole file")
            # "bytes 0-N/TOTAL": a file shorter than the range is sent whole.
            total = re.search(r"/(\d+)$", r.headers.get("Content-Range", ""))
            if total:
                wanted = min(wanted, int(total.group(1)))
            with open(tmp, "wb") as f:
                while chunk := r.read(1 << 20):
                    f.write(chunk)
    except (OSError, http.client.HTTPException) as e:
        discard(tmp)
        sys.exit(f"downloading {url} failed: {e}; run again to retry")
    got = os.path.getsize(tmp)
    if got != wanted:
        discard(tmp)
        sys.exit(f"{url} sent {got} bytes where {wanted} were asked for; run again to retry")
    os.replace(tmp, part)


def fetch(name, url, work, frames):
    """The clip's exact y4m reference and the SHA-256 of the bytes it came from."""
    base = os.path.join(work, name)
    part, ref, digest_file = base + ".part.y4m", base + ".ref.y4m", base + ".sha256"
    if os.path.exists(ref) and os.path.exists(digest_file):
        discard(part)
        with open(digest_file) as f:
            return ref, f.read().strip()
    # Whatever is here came from an interrupted run and is not to be trusted.
    discard(part, part + ".tmp", ref + ".tmp")
    download(url, part, frames)
    digest = sha256_of(part)
    run(["ffmpeg", "-y", "-v", "error", "-i", part, "-frames:v", str(frames),
         "-pix_fmt", "yuv420p", "-f", "yuv4mpegpipe", ref + ".tmp"])
    excess = os.path.getsize(ref + ".tmp") - frames * FRAME_BYTES
    if not 0 < excess < HEADER_ALLOWANCE:
        discard(part, ref + ".tmp")
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
    """The mean VMAF of `dist` against `ref`, and the libvmaf version that scored it."""
    log = dist + ".vmaf.json"
    if not os.path.exists(log):
        graph = (f"[0:v]scale={SRC_W}:{SRC_H}:flags=bicubic,setpts=PTS-STARTPTS[d];"
                 f"[1:v]setpts=PTS-STARTPTS[r];"
                 f"[d][r]libvmaf=log_fmt=json:log_path={log}.partial:n_threads={threads}")
        run(["ffmpeg", "-v", "error", "-i", dist, "-i", ref, "-lavfi", graph,
             "-f", "null", "-"])
        os.replace(log + ".partial", log)
    with open(log) as f:
        scored = json.load(f)
    return scored["pooled_metrics"]["vmaf"]["mean"], scored.get("version", "unknown")


def res_factor(res_scale, res_power, scale):
    octaves = 0.0 if scale >= 1 else math.log2(1 / scale)
    return 100 - min(100.0, res_scale * octaves ** res_power)


def artefact_factor(bpp_half, bpp_slope, bpp):
    artefact = 100.0 / (1.0 + (max(bpp, 1e-9) / bpp_half) ** bpp_slope)
    return 1 - artefact / 100


def predict(fit, scale, bpp):
    """Predicted VMAF: 100 minus budget.rs's video_loss."""
    res_scale, res_power, bpp_half, bpp_slope = fit
    return (res_factor(res_scale, res_power, scale)
            * artefact_factor(bpp_half, bpp_slope, bpp))


def fit(rows):
    """The grid point with the least squared error, and its rmse. The two
    factors of `predict` each depend on only two of the four fields, so each
    is computed once per pair, not once per point."""
    scales = [r["scale"] for r in rows]
    scores = [r["vmaf"] for r in rows]
    artefacts = {(bh, bs): [artefact_factor(bh, bs, r["bpp"]) for r in rows]
                 for bh in GRID["bpp_half"] for bs in GRID["bpp_slope"]}
    best = None
    for rs in GRID["res_scale"]:
        for rp in GRID["res_power"]:
            res = [res_factor(rs, rp, s) for s in scales]
            for (bh, bs), art in artefacts.items():
                err = sum((a * b - v) ** 2 for a, b, v in zip(res, art, scores))
                if best is None or err < best[0]:
                    best = (err, (rs, rp, bh, bs))
    err, f = best
    return f, math.sqrt(err / len(rows))


def clip_offsets(rows, best):
    """How much of the fit's error is which clip it is. Each clip's mean
    residual, and the rmse left once that one offset per clip is removed: the
    part of the error a model that knows nothing about content cannot remove,
    however its other constants are chosen."""
    residuals = {}
    for r in rows:
        residuals.setdefault(r["clip"], []).append(
            r["vmaf"] - predict(best, r["scale"], r["bpp"]))
    offsets = {clip: statistics.mean(es) for clip, es in residuals.items()}
    err = sum((e - offsets[clip]) ** 2 for clip, es in residuals.items() for e in es)
    return offsets, math.sqrt(err / len(rows))


def operating_region(rows, min_bpp):
    """The encodes the size budget is used for: large enough to say something,
    and aimed at a bits-per-pixel an encoder can hold. The small tolerance
    keeps a target of exactly min_bpp in."""
    return [r for r in rows
            if r["bytes"] >= MIN_STAT_BYTES and r["target_bpp"] >= min_bpp - 1e-9]


def size_stats(rows, min_bpp):
    """Median, 90th percentile and maximum of size over target, from the
    operating region; None when it is empty."""
    ratios = sorted(r["size_ratio"] for r in operating_region(rows, min_bpp))
    if not ratios:
        return None
    return {"used": len(ratios), "of": len(rows), "median": statistics.median(ratios),
            "p90": ratios[min(len(ratios) - 1, int(0.9 * len(ratios)))],
            "max": ratios[-1]}


def size_by_bpp(rows):
    """(target bpp, encodes, median, maximum) of size over target for each
    target bits per pixel, over the encodes of at least MIN_STAT_BYTES:
    where the overshoot is, whether or not it is in the operating region."""
    out = []
    for bpp in sorted({r["target_bpp"] for r in rows}):
        ratios = sorted(r["size_ratio"] for r in rows
                        if r["target_bpp"] == bpp and r["bytes"] >= MIN_STAT_BYTES)
        if ratios:
            out.append((bpp, len(ratios), statistics.median(ratios), ratios[-1]))
    return out


def permille(fraction):
    # Rounded first, so 0.03 is 30 and not 31 by way of 30.000000000000004.
    return math.ceil(round(fraction * 1000, 6))


def reserve(stats):
    """(codec, overhead, margin) in per mille, from the codec that overshoots
    the most; None when no codec has statistics. The whole reserve is that
    codec's p90 over target (never under MIN_RESERVE), the overhead is its
    median over target, and the margin is the rest."""
    usable = {codec: s for codec, s in stats.items() if s}
    if not usable:
        return None
    codec = max(usable, key=lambda c: usable[c]["p90"])
    s = usable[codec]
    total = max(MIN_RESERVE, s["p90"] - 1)
    overhead = permille(max(0.0, s["median"] - 1))
    return codec, overhead, max(0, permille(total) - overhead)


def parse_args(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True)
    ap.add_argument("--codecs", default="x264,vp9", help="x264, vp9 or both, comma-separated")
    ap.add_argument("--frames", type=int, default=250)
    ap.add_argument("--vp9-clips", default="in_to_tree",
                    help="VP9 is slow; calibrate it on these clips only")
    ap.add_argument("--reserve-min-bpp", type=float, default=DEFAULT_RESERVE_MIN_BPP,
                    help="size the reserve from encodes aimed at least this many bits per "
                         "pixel (default %(default)s); below that an encoder saturates and "
                         "the executor re-plans, so those encodes do not set the reserve")
    ap.add_argument("--threads", type=int, default=os.cpu_count() or 4)
    ap.add_argument("--verbose", action="store_true")
    args = ap.parse_args(argv)
    args.codecs = args.codecs.split(",")
    args.vp9_clips = args.vp9_clips.split(",")
    for codec in args.codecs:
        if codec not in CODECS:
            ap.error(f"--codecs: unknown codec {codec!r}; choose from {', '.join(CODECS)}")
    if len(set(args.codecs)) != len(args.codecs):
        ap.error("--codecs: a codec is listed twice")
    for clip in args.vp9_clips:
        if clip not in CLIPS:
            ap.error(f"--vp9-clips: unknown clip {clip!r}; choose from {', '.join(CLIPS)}")
    if len(set(args.vp9_clips)) != len(args.vp9_clips):
        ap.error("--vp9-clips: a clip is listed twice")
    if args.frames < 1:
        ap.error("--frames must be at least 1")
    if args.threads < 1:
        ap.error("--threads must be at least 1")
    if not args.reserve_min_bpp >= 0:
        ap.error("--reserve-min-bpp must not be negative")
    args.work = os.path.abspath(args.work)
    if any(c in args.work for c in FILTER_GRAPH_SPECIALS):
        ap.error(f"--work must not contain any of {FILTER_GRAPH_SPECIALS}")
    return args


def main():
    global VERBOSE
    args = parse_args()
    VERBOSE = args.verbose
    work = args.work
    ffmpeg_version = preflight(args.codecs)
    print(f"ffmpeg: {ffmpeg_version}", flush=True)
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

    rows = {}
    libvmaf = None
    for codec in args.codecs:
        rows[codec] = []
        clips = CLIPS if codec == "x264" else args.vp9_clips
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
                    score, version = vmaf(out, refs[name], args.threads)
                    if libvmaf is None:
                        libvmaf = version
                        print(f"libvmaf: {libvmaf}", flush=True)
                    rows[codec].append({
                        "clip": name, "codec": codec, "short": short, "w": w, "h": h,
                        "target_bpp": bpp, "bps": bps, "bytes": size,
                        "size_ratio": size / (bps * seconds / 8),
                        "scale": short / SRC_H, "bpp": actual_bpp, "vmaf": score,
                    })
                    print(f"{name} {codec} {w}x{h} bpp {bpp}: "
                          f"{score:.2f} vmaf, ratio {rows[codec][-1]['size_ratio']:.4f}, "
                          f"{time.monotonic() - began:.1f} s", flush=True)
        with open(os.path.join(work, f"results-{codec}.csv"), "w", newline="") as f:
            wr = csv.DictWriter(f, fieldnames=list(rows[codec][0].keys()))
            wr.writeheader()
            wr.writerows(rows[codec])

    print("\n// --- paste into crates/convkit-core/src/budget.rs ---")
    print(f"// measured with {ffmpeg_version}, libvmaf {libvmaf}")
    for codec, const in (("x264", "X264_FIT"), ("vp9", "VP9_FIT")):
        if codec not in rows:
            continue
        best, rmse = fit(rows[codec])
        rs, rp, bh, bs = best
        print(f"pub const {const}: VideoFit = VideoFit {{ res_scale: {rs:.1f}, "
              f"res_power: {rp:.2f}, bpp_half: {bh:.5f}, bpp_slope: {bs:.1f} }}; "
              f"// rmse {rmse:.2f} VMAF over {len(rows[codec])} encodes")
        offsets, left = clip_offsets(rows[codec], best)
        print(f"// {const}: one offset per clip would bring the rmse to {left:.2f}; the clips' "
              f"mean residuals are "
              + ", ".join(f"{clip} {offset:+.1f}" for clip, offset in offsets.items())
              + ". That spread at equal bits per pixel is content, which this model cannot see.")
        for field, value in zip(GRID, best):
            values = GRID[field]
            if value in (values[0], values[-1]):
                print(f"// note: {const} {field} is at the edge of its search range "
                      f"({values[0]}-{values[-1]}); the optimum may lie beyond it")
    min_bpp = args.reserve_min_bpp
    print(f"// The size reserve is sized from the operating region only: encodes of at least "
          f"{MIN_STAT_BYTES // 1024} KiB aimed at {min_bpp} bits per pixel or more.")
    print("// Below that the overshoot is concentrated in deliberately starved cells, where "
          "the encoder")
    print("// saturates (libx264 cannot spend fewer bits than its floor on so thin a "
          "budget); the")
    print("// executor re-plans at a smaller picture when an encode comes back over, so "
          "those cells")
    print("// are not a bias the reserve should pay for on every file.")
    for codec in rows:
        for bpp, n, median, worst in size_by_bpp(rows[codec]):
            print(f"// size over target, {codec}, target {bpp} bpp: median {median:.4f}, "
                  f"max {worst:.4f}, {n} encodes of at least {MIN_STAT_BYTES // 1024} KiB")
    stats = {codec: size_stats(rows[codec], min_bpp) for codec in rows}
    for codec, s in stats.items():
        if s:
            print(f"// size over target, {codec}, operating region: median {s['median']:.4f}, "
                  f"p90 {s['p90']:.4f}, max {s['max']:.4f}, from {s['used']} of "
                  f"{len(rows[codec])} encodes")
        else:
            print(f"// size over target, {codec}: no encode reached "
                  f"{MIN_STAT_BYTES // 1024} KiB at {min_bpp} bits per pixel, so none is used")
    chosen = reserve(stats)
    if chosen is None:
        print("// OVERHEAD_PERMILLE and MARGIN_PERMILLE: no codec has an encode in the "
              "operating region; run with more --frames or a lower --reserve-min-bpp")
    else:
        codec, overhead, margin = chosen
        s = stats[codec]
        print(f"// reserve set by {codec} alone, the larger p90 of the codecs run "
              f"({', '.join(rows)}), over {s['used']} encodes")
        print(f"pub const OVERHEAD_PERMILLE: u64 = {overhead}; "
              f"// {codec} median size over target {s['median']:.4f}")
        print(f"pub const MARGIN_PERMILLE: u64 = {margin}; "
              f"// {codec} p90 {s['p90']:.4f}, less the overhead")
    print(f"\n// finished in {(time.monotonic() - started) / 60:.1f} minutes")


if __name__ == "__main__":
    main()
