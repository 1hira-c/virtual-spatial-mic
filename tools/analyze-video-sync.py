#!/usr/bin/env python3
"""Decode developer AV fixture recordings; no nonstandard Python packages required."""
from __future__ import annotations
import argparse
import array
import json
import math
import os
import re
from pathlib import Path
import shutil
import subprocess
import sys


def run(command: list[str]) -> bytes:
    result = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            timeout=120, creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
    if result.returncode:
        raise RuntimeError(f"{command[0]} failed ({result.returncode}):\n{result.stderr.decode(errors='replace')[-8000:]}")
    return result.stdout


def audio_onset(samples, expected: float, rate: int, pulse_frames: int) -> dict:
    first = max(0, round((expected - .25) * rate))
    last = min(len(samples) - pulse_frames, round((expected + .25) * rate))
    if last <= first:
        raise RuntimeError(f"Not enough decoded audio around pulse {expected}s")
    # 1 kHz at 48 kHz is exactly 48 samples/cycle. Sliding complex projections
    # evaluate every candidate onset in O(n), not a frame-quantized envelope.
    cycle = rate // 1000
    cosine = [math.cos(math.tau * i / cycle) for i in range(cycle)]
    sine = [math.sin(math.tau * i / cycle) for i in range(cycle)]
    pc, ps, pe = [0.0], [0.0], [0.0]
    for i in range(first, last + pulse_frames):
        sample = float(samples[i])
        pc.append(pc[-1] + sample * cosine[i % cycle])
        ps.append(ps[-1] + sample * sine[i % cycle])
        pe.append(pe[-1] + sample * sample)
    best, best_score = first, -math.inf
    for lag in range(first, last + 1):
        offset = lag - first
        score = (cosine[lag % cycle] * (pc[offset + pulse_frames] - pc[offset])
                 + sine[lag % cycle] * (ps[offset + pulse_frames] - ps[offset]))
        if score > best_score:
            best, best_score = lag, score
    offset = best - first
    energy = pe[offset + pulse_frames] - pe[offset]
    correlation = best_score / math.sqrt(pulse_frames / 2 * energy) if energy > 0 else 0
    # Also expose first audible energy; AAC pre-echo can lead the matched onset.
    rms_window = rate // 1000
    threshold = .07
    audible = None
    for lag in range(first, min(last, best + pulse_frames) + 1):
        offset = lag - first
        if math.sqrt(max(0, (pe[offset + rms_window] - pe[offset]) / rms_window)) >= threshold:
            audible = lag / rate
            break
    return {"matched_time_s": best / rate, "matched_sample": best,
            "correlation": correlation, "rms_threshold_time_s": audible,
            "rms_window_frames": rms_window, "rms_threshold": threshold,
            "amplitude_estimate": best_score * 2 / pulse_frames}


def audio_markers(samples, rate: int) -> list[dict]:
    window = rate // 1000
    threshold = .07
    power = sum(float(v)**2 for v in samples[:window])
    last_marker = -rate
    active = False
    markers = []
    for first in range(max(0, len(samples)-window+1)):
        above = math.sqrt(max(0, power/window)) >= threshold
        if above and not active and first-last_marker >= rate//4:
            markers.append({"sample": first, "time_s": first/rate})
            last_marker = first
        active = above
        if first+window < len(samples):
            power += float(samples[first+window])**2 - float(samples[first])**2
    return markers


def fit_drift(pairs: list[tuple[float, float]]) -> dict | None:
    if len(pairs) < 2:
        return None
    mx = sum(x for x, _ in pairs) / len(pairs)
    my = sum(y for _, y in pairs) / len(pairs)
    slope = sum((x-mx)*(y-my) for x, y in pairs) / sum((x-mx)**2 for x, _ in pairs)
    return {"offset_at_t0_ms": my - slope*mx, "drift_ms_per_second": slope}


def analyze(manifest_path: Path, ffmpeg: str, ffprobe: str) -> dict:
    manifest = json.loads(manifest_path.read_text(encoding='utf-8-sig'))
    if manifest.get('schema') != 'vsm-av-fixture-1':
        raise RuntimeError(f"Unsupported manifest: {manifest_path}")
    mp4 = Path(manifest['mp4'])
    if not mp4.is_absolute():
        # The fixture can be run with a relative output directory; its manifest
        # may contain that full relative path rather than only the basename.
        candidates = [manifest_path.parent/mp4, mp4, manifest_path.parent/mp4.name]
        mp4 = next((candidate for candidate in candidates if candidate.is_file()), candidates[0])
    origin = int(manifest['recording_origin_qpc_ns'])
    probe = json.loads(run([ffprobe, '-v', 'error', '-select_streams', 'v:0',
                           '-show_frames', '-show_entries', 'frame=best_effort_timestamp_time,pts_time',
                           '-of', 'json', str(mp4)]))
    pts = [float(f.get('best_effort_timestamp_time', f.get('pts_time')))
           for f in probe.get('frames', [])
           if 'best_effort_timestamp_time' in f or 'pts_time' in f]
    gray = run([ffmpeg, '-hide_banner', '-v', 'error', '-i', str(mp4), '-map', '0:v:0',
                '-an', '-vf', 'scale=1:1:flags=area,format=gray', '-fps_mode', 'passthrough',
                '-f', 'rawvideo', 'pipe:1'])
    if len(gray) != len(pts):
        raise RuntimeError(f"Decoded frame count {len(gray)} != probed PTS count {len(pts)}")
    raw = run([ffmpeg, '-hide_banner', '-v', 'error', '-i', str(mp4), '-map', '0:a:0', '-vn',
               '-af', 'aresample=async=1:first_pts=0', '-ar', str(manifest['audio_rate']),
               '-ac', '1', '-f', 'f32le', 'pipe:1'])
    samples = array.array('f')
    samples.frombytes(raw)
    if sys.byteorder != 'little':
        samples.byteswap()
    if not gray:
        raise RuntimeError('No decoded video frames')
    low, high = min(gray), max(gray)
    if high-low < 100:
        raise RuntimeError(f"No clear black/white flash range: {low}..{high}")
    threshold = (low+high)/2
    visual = []
    was_white = False
    for i, (time, value) in enumerate(zip(pts, gray)):
        white = value >= threshold
        if white and not was_white:
            visual.append({"frame": i, "time_s": time, "luma": value})
        was_white = white
    source = {int(f['index']): f for f in manifest['source_flash_events'] if f['white']}
    audible = audio_markers(samples, int(manifest['audio_rate']))
    # Pair by stimulus sequence only when every marker is present. Nearest-to-one-
    # second pairing aliases a genuine large delay to the next/previous marker.
    complete_sequence = len(visual) == len(audible) == len(manifest['audio_pulses']) == 6 and all(i in source for i in range(1, 7))
    events, missing = [], []
    if not complete_sequence:
        missing = [int(p['index']) for p in manifest['audio_pulses']]
    for order, pulse in enumerate(manifest['audio_pulses'] if complete_sequence else []):
        index = int(pulse['index'])
        expected = int(pulse['sample']) / int(manifest['audio_rate'])
        audio = audio_onset(samples, audible[order]['time_s'], int(manifest['audio_rate']), int(pulse['duration_frames']))
        video = visual[order]
        sent = source[index]
        publish_begin = (int(sent['publish_begin_qpc_ns'])-origin)/1e9
        publish_end = (int(sent['published_qpc_ns'])-origin)/1e9
        audio_t, video_t = audio['matched_time_s'], video['time_s']
        events.append({"index": index, "expected_time_s": expected, "audio": audio,
                       "video": video, "source_publication_begin_s": publish_begin,
                       "source_publication_end_s": publish_end,
                       "sender_lateness_ms": (publish_end-expected)*1000,
                       "audio_expected_error_ms": (audio_t-expected)*1000,
                       "video_expected_error_ms": (video_t-expected)*1000,
                       "video_vs_publication_begin_ms": (video_t-publish_begin)*1000,
                       "video_vs_publication_end_ms": (video_t-publish_end)*1000,
                       "video_minus_audio_ms": (video_t-audio_t)*1000})
    errors = [e['video_minus_audio_ms'] for e in events]
    return {"manifest": str(manifest_path), "scenario": manifest['scenario'],
            "decoded_video_frames": len(gray), "decoded_audio_frames": len(samples),
            "first_video_pts_s": pts[0] if pts else None,
            "last_video_pts_s": pts[-1] if pts else None,
            "luma_range": [low, high], "flash_threshold": threshold,
            "detected_flash_onsets": visual, "detected_audio_onsets": audible,
            "pairing": "sequence-order-with-exactly-six-markers" if complete_sequence else "ambiguous-marker-count",
            "missing_pulses": missing, "events": events,
            "all_six_pulses_measured": len(events) == 6 and not missing,
            "minimum_audio_correlation": min((e['audio']['correlation'] for e in events), default=0),
            "mean_video_minus_audio_ms": sum(errors)/len(errors) if errors else None,
            "max_absolute_video_minus_audio_ms": max(map(abs, errors), default=None),
            "audio_clock_fit": fit_drift([(e['expected_time_s'], e['audio_expected_error_ms']) for e in events]),
            "av_fit": fit_drift([(e['expected_time_s'], e['video_minus_audio_ms']) for e in events]),
            "recorder": manifest['recorder'],
            "measurement_note": "MP4 playback PTS; audio uses first_pts=0 alignment and exact 1kHz matched filtering. Video is quantized to 30fps; source-publication brackets separate source scheduling/mutex delay. AAC pre-echo is reported separately by the RMS onset."}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument('input', type=Path, help='manifest JSON or directory of av-*.json')
    parser.add_argument('--ffmpeg', default=shutil.which('ffmpeg'))
    parser.add_argument('--ffprobe', default=shutil.which('ffprobe'))
    parser.add_argument('--output', type=Path)
    parser.add_argument('--max-av-ms', type=float, help='optional acceptance limit; no guessed default')
    args = parser.parse_args()
    if not args.ffmpeg:
        parser.error('Provide --ffmpeg path; this tool never downloads codecs.')
    if not args.ffprobe:
        companion = Path(args.ffmpeg).with_name('ffprobe.exe' if os.name == 'nt' else 'ffprobe')
        if companion.is_file():
            args.ffprobe = str(companion)
        else:
            parser.error('Provide --ffprobe path.')
    manifests = sorted(p for p in args.input.glob('av-*.json') if re.fullmatch(r'av-(?:normal|mutex20|mutex60)\.json', p.name)) if args.input.is_dir() else [args.input]
    if not manifests:
        parser.error('No av-*.json manifests found.')
    results = [analyze(path, args.ffmpeg, args.ffprobe) for path in manifests]
    report = {"schema": "vsm-av-fixture-analysis-1", "cases": results}
    destination = args.output or (args.input / 'av-analysis.json' if args.input.is_dir() else args.input.with_name(args.input.stem+'-analysis.json'))
    destination.write_text(json.dumps(report, ensure_ascii=False, indent=2)+'\n', encoding='utf-8')
    for result in results:
        print(f"{result['scenario']}: pulses={len(result['events'])}/6, correlation={result['minimum_audio_correlation']:.4f}, mean(V-A)={result['mean_video_minus_audio_ms']} ms, max|V-A|={result['max_absolute_video_minus_audio_ms']} ms")
        for event in result['events']:
            print(f"  {event['index']}: audio_err={event['audio_expected_error_ms']:.3f} ms; video_err={event['video_expected_error_ms']:.3f} ms; source_late={event['sender_lateness_ms']:.3f} ms; V-A={event['video_minus_audio_ms']:.3f} ms")
    print(f"report={destination}")
    complete = all(r['all_six_pulses_measured'] and r['minimum_audio_correlation'] >= .7 for r in results)
    within = args.max_av_ms is None or all(r['max_absolute_video_minus_audio_ms'] is not None and r['max_absolute_video_minus_audio_ms'] <= args.max_av_ms for r in results)
    return 0 if complete and within else 1


if __name__ == '__main__':
    raise SystemExit(main())
