#!/usr/bin/env python3
"""Measure the release renderer and all its descendants using only stdlib.

Usage: python3 scripts/measure-render.py OUTPUT_PREFIX BINARY [ROUNDS] [cached]
CPU/RSS are sampled every 5 ms. RAPL energy is recorded when readable.
No mailbox access; the binary supplies deterministic offline fixtures.
"""
import json
import os
from pathlib import Path
import statistics
import subprocess
import sys
import time

prefix, binary, *options = sys.argv[1:]
energy_path = Path('/sys/class/powercap/intel-rapl:0/energy_uj')


def energy():
    try:
        return int(energy_path.read_text())
    except (OSError, ValueError):
        return None


cpu_ticks = {}
peak_rss = 0
start_energy = energy()
start = time.monotonic()
with open(prefix + '.jsonl', 'w') as output:
    process = subprocess.Popen([binary, '--bench-render', *(options or ['8'])], stdout=output)
    while process.poll() is None:
        pending = [process.pid]
        seen = set()
        rss = 0
        while pending:
            pid = pending.pop()
            if pid in seen:
                continue
            seen.add(pid)
            try:
                stat = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
                cpu_ticks[pid] = max(cpu_ticks.get(pid, 0), int(stat[11]) + int(stat[12]))
                rss += int(stat[21]) * os.sysconf('SC_PAGE_SIZE')
                for task in Path(f'/proc/{pid}/task').iterdir():
                    try:
                        pending.extend(map(int, (task / 'children').read_text().split()))
                    except OSError:
                        pass
            except (OSError, ValueError, IndexError):
                pass
        peak_rss = max(peak_rss, rss)
        time.sleep(.005)
elapsed = time.monotonic() - start
end_energy = energy()
if process.returncode:
    sys.exit(process.returncode)
records = [json.loads(line) for line in Path(prefix + '.jsonl').read_text().splitlines()]
cases = {}
for name in dict.fromkeys(row['case'] for row in records):
    samples = sorted(row['ms'] for row in records if row['case'] == name)
    cases[name] = {'median_ms': statistics.median(samples), 'min_ms': min(samples),
                   'p95_ms': samples[min(len(samples)-1, int(len(samples)*.95))]}
summary = {'cases': cases, 'elapsed_s': elapsed,
           'sampled_cpu_s': sum(cpu_ticks.values()) / os.sysconf('SC_CLK_TCK'),
           'peak_tree_rss_mib': peak_rss / 1024**2,
           'processes_seen': len(cpu_ticks),
           'package_joules': ((end_energy-start_energy)/1e6
                              if start_energy is not None and end_energy is not None else None),
           'energy_note': 'RAPL unavailable without permission' if start_energy is None else 'System package, includes other apps'}
Path(prefix + '.json').write_text(json.dumps(summary, indent=2) + '\n')
print(json.dumps(summary, indent=2))
