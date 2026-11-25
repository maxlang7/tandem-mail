# Email rendering measurements

These measurements cover the previous Brave screenshot reader.

Measured November 20, 2026 on Aragorn, a Framework Laptop 13 with an Intel
Core i5-1135G7, using the project's unchanged optimized release profile.

The 10x target is met for saved renders and the short-notification median.
Fresh large messages improve about 5x. Equal wattage has not been verified.

## Results

Each median uses 16 offline samples. Timings include MIME sanitization and
PNG decoding, and end before egui texture upload. Network downloads are excluded.

| Fixture | Before | Fresh render | Saved render | Fresh speedup | Saved speedup |
| --- | ---: | ---: | ---: | ---: | ---: |
| Notification, 1000 px, 1x | 538.96 ms | 47.05 ms | 2.78 ms | 11.46x | 194.12x |
| Newsletter, 1000 px, 1x | 748.37 ms | 138.22 ms | 27.20 ms | 5.41x | 27.51x |
| High density, 800 px, 2x | 732.75 ms | 146.52 ms | 23.29 ms | 5.00x | 31.46x |

The fresh run rendered different content in every round, including one cold
browser startup. That first sample still took 390.78 ms. It does not become 10x
faster after the worker has shut down. The saved-render run seeded a new cache
before timing lookups. Existing in-memory reader textures already open without
this preparation; these numbers measure the path after their eviction or an
application restart.

Across 48 fresh renders, sampled process-tree CPU time fell from 48.03 to
6.84 seconds, a reduction of 85.76%. Browser process creation fell from one
browser per message to one per batch. The sampler visits children of every
thread, including Tokio worker threads, and samples every 5 ms. It can miss
short-lived work. Summed process RSS double-counts shared memory, so it is not
an estimate of physical RAM use. The temporary browser still uses substantial
memory while active.

While the worker was frozen, its process tree consumed no measurable CPU time
between the fifth and tenth idle seconds, at 10 ms accounting resolution.
At 17 seconds, the browser had exited and only the benchmark process remained.
The laptop was charging and package-energy counters were root-readable only.
These measurements do not establish equal instantaneous or average wattage.
Other desktop applications were running during the measurements.

## Changes

- Reuse one private Brave process and page instead of starting a browser for
  every message. HTTP and file requests remain blocked in that browser.
- Capture the full document once with fast, lossless PNG encoding, then split
  the decoded image into texture tiles. Preserve pixel density and link geometry.
- Freeze the page between jobs and terminate the browser after 15 idle seconds.
  Discard interrupted sessions so a clicked message does not inherit a canceled
  preload's DevTools conversation.
- Save finished captures in a separate, private SQLite cache with a 128 MiB data
  budget and 256-entry limit. Its keys include sanitized content, image bytes,
  width, and density. Corrupt entries fall back to a fresh render.

Hardware GPU capture and lossless WebP were profiled but not adopted. They did
not provide consistent gains in these tests. Chromium's screenshot encoder
[uses lossless WebP at quality 100](https://chromium.googlesource.com/chromium/src/+/refs/heads/main/content/browser/devtools/protocol/page_handler.cc),
but its encoding cost was higher here.

## Reproduce

The pre-optimization source is checkpointed at `5009839`. The retained baseline
binary additionally includes the benchmark entry point and fixtures, without
renderer changes. All outputs below live in the ignored Cargo target directory.

```sh
nix-shell --run 'cargo build --release --locked -j 3'
python3 scripts/measure-render.py target/render-final-baseline target/tandem-render-baseline 16
python3 scripts/measure-render.py target/render-final-fresh target/release/tandem-mail 16
python3 scripts/measure-render.py target/render-final-cached target/release/tandem-mail 16 cached
TANDEM_BENCH_IDLE=20 target/release/tandem-mail --bench-render 1
```

The JSONL files retain individual samples. Companion JSON files summarize them.
Cached mode's total process CPU includes its untimed seed renders.

Validation passed: 26 regular tests, five explicitly enabled browser tests,
strict Clippy, formatting, a local desktop preview, and the release build.
Browser tests cover exact pixels, links, content replacement, cancellation,
idle shutdown, cache corruption recovery, and prepared reader textures.
The unrelated test requiring a signed-in Luna CLI was left disabled.
