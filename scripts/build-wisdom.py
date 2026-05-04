#!/usr/bin/env python3
"""Build FFTW single-precision wisdom in parallel into /etc/fftw/wisdomf.

Splits a list of FFT problem specifiers across N parallel `fftwf-wisdom`
processes using LPT (Longest Processing Time first) bin-packing weighted
by N*log2(N), then merges the partial wisdoms into the final file.

Default problem list matches ka9q-radio RX888 needs; override with
positional args if your build differs.

Run with sudo if writing to /etc/fftw/wisdomf.
"""
import argparse
import math
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

DEFAULT_PROBLEMS = [
    "rof3240000", "rof1620000", "rof500000", "cof36480",
    "cob1920", "cob1200", "cob960", "cob800", "cob600",
    "cob480", "cob320", "cob300", "cob200", "cob160", "cob150",
]
DEFAULT_OUTPUT = "/etc/fftw/wisdomf"


def cost(spec: str) -> float:
    m = re.search(r"\d+", spec)
    if not m:
        return 1.0
    n = int(m.group())
    return n * math.log2(max(n, 2))


def lpt_split(problems, k):
    """Longest Processing Time scheduling into k bins."""
    bins = [[] for _ in range(k)]
    loads = [0.0] * k
    for p in sorted(problems, key=cost, reverse=True):
        i = min(range(k), key=lambda j: loads[j])
        bins[i].append(p)
        loads[i] += cost(p)
    return [(b, l) for b, l in zip(bins, loads) if b]


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("-w", "--workers", type=int, default=os.cpu_count() or 4,
                   help="parallel processes (default: cpu_count)")
    p.add_argument("-o", "--output", default=DEFAULT_OUTPUT,
                   help=f"final wisdom path (default: {DEFAULT_OUTPUT})")
    p.add_argument("--impatient", action="store_true",
                   help="IMPATIENT planning mode (faster, slightly suboptimal)")
    p.add_argument("--dry-run", action="store_true",
                   help="print plan and exit")
    p.add_argument("problems", nargs="*",
                   help="FFT problem specifiers (default: ka9q-radio RX888 set)")
    args = p.parse_args()

    problems = args.problems or DEFAULT_PROBLEMS
    plan = lpt_split(problems, args.workers)

    print(f"Plan: {len(plan)} workers, {len(problems)} problems")
    for i, (chunk, load) in enumerate(plan):
        print(f"  worker {i}: cost={load:.2e} | {' '.join(chunk)}")
    if args.dry_run:
        return

    output = Path(args.output)
    if output.exists():
        bak = output.with_suffix(output.suffix + ".bak")
        try:
            shutil.copy2(output, bak)
            print(f"Backup: {output} -> {bak}")
        except PermissionError as e:
            print(f"WARN: could not backup existing wisdom: {e}", file=sys.stderr)

    tmp = Path(tempfile.mkdtemp(prefix="fftw-wisdom-"))
    parts = []
    procs = []

    def cleanup(signum=None, frame=None):
        print("\nInterrupted: terminating workers", file=sys.stderr)
        for proc in procs:
            if proc.poll() is None:
                proc.terminate()
        for proc in procs:
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
        sys.exit(130 if signum else 1)

    signal.signal(signal.SIGINT, cleanup)
    signal.signal(signal.SIGTERM, cleanup)

    base = ["fftwf-wisdom", "-v", "-T", "1"]
    if args.impatient:
        base.append("-i")

    print()
    t0 = time.monotonic()
    for i, (chunk, _) in enumerate(plan):
        part = tmp / f"part{i}"
        parts.append(part)
        cmd = [*base, "-o", str(part), *chunk]
        print(f"$ {' '.join(cmd)}")
        procs.append(subprocess.Popen(cmd))

    rc = 0
    for proc in procs:
        rc |= proc.wait()
    elapsed = time.monotonic() - t0
    print(f"\nWorkers done in {elapsed:.1f}s (rc={rc})")
    if rc != 0:
        print("FAILED: at least one worker exited non-zero", file=sys.stderr)
        sys.exit(1)

    try:
        output.parent.mkdir(parents=True, exist_ok=True)
    except PermissionError as e:
        print(f"FAILED: cannot create {output.parent}: {e}", file=sys.stderr)
        print("Re-run with sudo.", file=sys.stderr)
        sys.exit(1)

    merge = ["fftwf-wisdom"]
    for part in parts:
        merge += ["-w", str(part)]
    merge += ["-o", str(output)]
    print(f"\n$ {' '.join(merge)}")
    try:
        subprocess.run(merge, check=True)
    except subprocess.CalledProcessError as e:
        print(f"FAILED: merge step exited {e.returncode}", file=sys.stderr)
        sys.exit(1)
    except PermissionError:
        print(f"FAILED: cannot write to {output}. Re-run with sudo.", file=sys.stderr)
        sys.exit(1)

    for part in parts:
        part.unlink(missing_ok=True)
    tmp.rmdir()

    size = output.stat().st_size
    print(f"\nDone: {output} ({size} bytes)")


if __name__ == "__main__":
    main()
