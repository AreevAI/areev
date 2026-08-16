#!/usr/bin/env python3
"""Edge-device benchmark: run Areev's read/write/vector paths on the box itself
and print the markdown rows that go into RESULTS.md §6.

    usage:  edge_bench.py [--sizes 500,2000,8000] [--vector] [--ingest 200]
    needs:  pip install areev          (the wheel — never build on-device)
            pip install fastembed       (only for --vector)

Written for a Raspberry Pi but device-agnostic: on a Pi it samples the ARM
clock *throughout* every phase, so a run taken at a reduced clock is reported
as such instead of being published as the platform's speed. Elsewhere the
certification is skipped and the timings still stand.

Three measurement rules this harness enforces, all learned the hard way:
  * recall benchmarks assert the query HITS. Timing a subject that was never
    imported measures the miss path (~10x faster) and reads as a great result.
  * power is sampled DURING a phase, never after. The clock recovers to nominal
    within milliseconds of the load ending, so an after-the-fact reading calls
    a fully throttled run "clean".
  * the vector legs report model and scan separately. On slow silicon the
    embedder is ~95% of end-to-end latency, so a single blended number tells
    you nothing about the engine.
"""
import argparse
import hashlib
import json
import os
import statistics
import subprocess
import tempfile
import threading
import time

import areev

DIM = 384  # MiniLM / bge-small width, so the fake vectors cost realistic float work
SYSFS_FREQ = "/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq"


# ---------------------------------------------------------------- environment
def vcgencmd(arg):
    """Return a vcgencmd reading, or None off-Pi."""
    try:
        out = subprocess.run(["vcgencmd"] + arg.split(), capture_output=True,
                             text=True, timeout=5)
        return out.stdout.strip().split("=", 1)[1] if "=" in out.stdout else None
    except (FileNotFoundError, subprocess.SubprocessError, IndexError):
        return None


def throttled():
    """(raw, [live conditions]) — the sticky 'since boot' bits are ignored here
    because they survive warm reboots and only clear on a power cycle."""
    raw = vcgencmd("get_throttled")
    if raw is None:
        return None, []
    t = int(raw, 16)
    live = [label for bit, label in
            [(0, "under-voltage"), (1, "freq-capped"), (2, "throttled"), (3, "soft-temp-limit")]
            if t >> bit & 1]
    return raw, live


class Certify:
    """Sample clock + throttle flags throughout a phase and report the worst case.

    On a Pi the clock MUST come from `vcgencmd measure_clock arm`, not from
    sysfs. `scaling_cur_freq` is the governor's *requested* frequency: when the
    VideoCore firmware throttles it drops the real clock without telling Linux,
    so sysfs happily reports a flat 1200 MHz through a run that actually
    executed at 600. Sampling costs a fork per second, which is negligible next
    to the phases being measured.
    """

    def __init__(self, phase):
        self.phase = phase
        self.freqs = []
        self.live = set()
        self.source = "vcgencmd" if vcgencmd("measure_clock arm") else (
            "sysfs (governor request — not authoritative under firmware throttling)"
            if os.path.exists(SYSFS_FREQ) else None)
        self._stop = False
        self._thread = None

    def _read_clock(self):
        if self.source == "vcgencmd":
            v = vcgencmd("measure_clock arm")
            return int(v) // 10**6 if v else None
        try:
            with open(SYSFS_FREQ) as fh:
                return int(fh.read()) // 1000
        except OSError:
            return None

    def _sample(self):
        last_flag_check = 0.0
        while not self._stop:
            mhz = self._read_clock()
            if mhz:
                self.freqs.append(mhz)
            now = time.monotonic()
            if now - last_flag_check > 2.0:
                _, live = throttled()
                self.live.update(live)
                last_flag_check = now
            time.sleep(1.0 if self.source == "vcgencmd" else 0.25)

    def __enter__(self):
        if self.source:
            self._thread = threading.Thread(target=self._sample, daemon=True)
            self._thread.start()
        return self

    def __exit__(self, *exc):
        self._stop = True
        if self._thread:
            self._thread.join(timeout=2)
        if self.freqs:
            lo, mid, hi = min(self.freqs), statistics.median(self.freqs), max(self.freqs)
            verdict = (f"LIVE {', '.join(sorted(self.live))} — timings NOT at rated clock"
                       if self.live else "clean, at rated clock")
            print(f"\n_{self.phase}: clock during phase min {lo} / median {mid:.0f} / "
                  f"max {hi} MHz over {len(self.freqs)} samples — {verdict}_")
        return False


def rss_mib():
    return int(open("/proc/self/status").read().split("VmRSS:")[1].split()[0]) // 1024


def pct(values, q):
    s = sorted(values)
    return s[min(len(s) - 1, int(len(s) * q))]


# ------------------------------------------------------------------- fixtures
_QUERY_VECTOR = {}


def fake_embed(text):
    """Deterministic 384-dim vector. Only the query is cached (in
    _QUERY_VECTOR) — caching every text would put ~12 KB of Python floats per
    grain in RSS and get misread as Areev's own footprint."""
    hit = _QUERY_VECTOR.get(text)
    if hit is not None:
        return hit
    out, i = [], 0
    while len(out) < DIM:
        out.extend(b / 255.0 for b in hashlib.sha256(f"{text}|{i}".encode()).digest())
        i += 1
    return out[:DIM]


def corpus(start, end):
    """JSONL with distinct subjects, so recall is a point lookup, not a ranking
    of hundreds of grains that happen to share one subject."""
    return "\n".join(json.dumps({"subject": f"person{i}", "relation": "prefers",
                                 "object": f"drink{i % 9} variant {i}"})
                     for i in range(start, end))


# --------------------------------------------------------------------- phases
def read_path(sizes):
    print("\n### Read path (bulk-loaded, FTS index live)\n")
    print("| grains | recall hit p50 | recall hit p99 | recall miss p50 | `latest()` p50 | CAL p50 | RSS |")
    print("|---|---|---|---|---|---|---|")
    with Certify("read path"):
        d = tempfile.mkdtemp()
        m = areev.Areev(os.path.join(d, "read.db"), ns="caller")
        prev = 0
        for n in sizes:
            m.migrate("jsonl", corpus(prev, n))
            prev = n
            assert json.loads(m.recall("person42", k=8)), "benchmark bug: query must HIT"
            hit, miss, latest, cal = [], [], [], []
            for _ in range(200):
                t = time.perf_counter(); json.loads(m.recall("person42", k=8)); hit.append(time.perf_counter() - t)
                t = time.perf_counter(); json.loads(m.recall("nobody-here", k=8)); miss.append(time.perf_counter() - t)
                t = time.perf_counter(); m.latest("person42", "prefers"); latest.append(time.perf_counter() - t)
            for _ in range(50):
                t = time.perf_counter(); m.cal('RECALL FACTS ABOUT "person42"'); cal.append(time.perf_counter() - t)
            print(f"| {n:,} | {pct(hit,.5)*1e6:.0f} µs | {pct(hit,.99)*1e6:.0f} µs | "
                  f"{pct(miss,.5)*1e6:.0f} µs | {pct(latest,.5)*1e6:.0f} µs | "
                  f"{pct(cal,.5)*1e3:.2f} ms | {rss_mib()} MiB |")


def write_path(sizes):
    print("\n### Write path — the edge-critical difference\n")
    print("| path | cost |")
    print("|---|---|")
    with Certify("write path"):
        d = tempfile.mkdtemp()
        w = areev.Areev(os.path.join(d, "single.db"), ns="caller")
        t0 = time.perf_counter()
        for i in range(30):
            w.add_fact(f"u{i}", "likes", f"x{i}")
        fresh = (time.perf_counter() - t0) / 30 * 1e3
        for i in range(30, 500):
            w.add_fact(f"u{i}", "likes", f"x{i}")
        t0 = time.perf_counter()
        for i in range(500, 520):
            w.add_fact(f"u{i}", "likes", f"x{i}")
        aged = (time.perf_counter() - t0) / 20 * 1e3
        print(f"| `add_fact`, one txn each, FTS live (empty file) | {fresh:.1f} ms/grain |")
        print(f"| `add_fact`, one txn each, FTS live (@500 grains) | {aged:.1f} ms/grain |")

        b = areev.Areev(os.path.join(d, "bulk.db"), ns="caller")
        prev = 0
        for n in sizes:
            t0 = time.perf_counter()
            b.migrate("jsonl", corpus(prev, n))
            per = (time.perf_counter() - t0) / (n - prev) * 1e3
            prev = n
            print(f"| `migrate` bulk import, index deferred (to {n:,}) | {per:.2f} ms/grain |")


def vector_path(ingest_n):
    print("\n### Vector recall — Areev's scan vs the embedding model\n")
    d = tempfile.mkdtemp()
    query = "who prefers tea in the morning"
    _QUERY_VECTOR[query] = fake_embed(query)

    print("| corpus | Areev vector scan p50 | p99 |")
    print("|---|---|---|")
    with Certify("vector scan"):
        m = areev.Areev(os.path.join(d, "scan.db"), ns="caller")
        m.set_embedder(fake_embed, "fake-384")
        prev = 0
        for n in (500, 2000):
            m.migrate("jsonl", corpus(prev, n))
            prev = n
            lat = []
            for _ in range(30):
                t = time.perf_counter(); json.loads(m.nearest(query, k=5)); lat.append(time.perf_counter() - t)
            print(f"| {n:,} grains | {pct(lat,.5)*1e3:.1f} ms | {pct(lat,.99)*1e3:.1f} ms |")

    print("\n| all-MiniLM-L6-v2 (ONNX, in-process) | value |")
    print("|---|---|")
    with Certify("embedding model"):
        base = rss_mib()
        t0 = time.perf_counter()
        from fastembed import TextEmbedding
        model = TextEmbedding("sentence-transformers/all-MiniLM-L6-v2")
        list(model.embed(["warmup"]))
        load_s, model_rss = time.perf_counter() - t0, rss_mib() - base
        texts = [f"john prefers {x} before the morning standup" for x in
                 ["tea", "coffee", "oat milk", "juice", "water", "matcha", "espresso", "chai"]]
        lat = []
        for i in range(20):
            t = time.perf_counter(); list(model.embed([texts[i % len(texts)]])); lat.append(time.perf_counter() - t)
        t0 = time.perf_counter()
        list(model.embed(texts * 4))
        batched = (time.perf_counter() - t0) / (len(texts) * 4) * 1e3
        print(f"| load time / resident cost | {load_s:.1f} s / +{model_rss} MiB |")
        print(f"| embed one query p50 | {pct(lat,.5)*1e3:.0f} ms |")
        print(f"| embed batched | {batched:.0f} ms/text |")

    print("\n| end-to-end (model + scan) | value |")
    print("|---|---|")
    with Certify("end-to-end vector recall"):
        m2 = areev.Areev(os.path.join(d, "real.db"), ns="caller")
        m2.set_embedder(lambda t: [float(x) for x in list(model.embed([t]))[0]], "minilm")
        t0 = time.perf_counter()
        for i in range(ingest_n):
            m2.add_fact(f"person{i}", "prefers", texts[i % len(texts)])
        ingest_ms = (time.perf_counter() - t0) / ingest_n * 1e3
        lat = []
        for _ in range(10):
            t = time.perf_counter(); json.loads(m2.nearest("who likes tea", k=5)); lat.append(time.perf_counter() - t)
        print(f"| `nearest()` @{ingest_n} grains | {pct(lat,.5)*1e3:.0f} ms p50 |")
        print(f"| ingest with on-device embedding | {ingest_ms:.0f} ms/grain |")
        print(f"| process RSS, model resident | {rss_mib()} MiB |")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sizes", default="500,2000,8000")
    ap.add_argument("--vector", action="store_true", help="include the embedding-model legs")
    ap.add_argument("--ingest", type=int, default=200, help="grains for the end-to-end vector leg")
    args = ap.parse_args()
    sizes = [int(s) for s in args.sizes.split(",")]

    total_mib = int(open("/proc/meminfo").readline().split()[1]) // 1024
    raw, _ = throttled()
    print(f"areev {areev.__version__} · {os.uname().machine} · {os.cpu_count()} cores · "
          f"{total_mib} MiB RAM" + (f" · get_throttled={raw} at start" if raw else ""))

    read_path(sizes)
    write_path(sizes)
    if args.vector:
        vector_path(args.ingest)


if __name__ == "__main__":
    main()
