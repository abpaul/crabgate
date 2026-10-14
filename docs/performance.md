# Performance measurements

Measured on 30 September 2026. Host: MacBookPro18,3, Apple Silicon, 8 logical CPUs, 16 GiB RAM. The release image ran in Docker Desktop's Linux/aarch64 VM, limited to 2 vCPUs (`docker update --cpus=2` or `docker run --cpus 2`). The VM reported 2 CPUs and 3.81 GiB available RAM. Requests came from a Python 3 standard-library HTTP client on the host over `localhost:5160`, with 50 persistent connections. The fixture was one published Event with one Ticket Type and no Orders or Holds. Every measured response was HTTP 200 with a nonempty HTML body. Results include Docker Desktop network and client overhead, and do not predict internet latency or larger datasets.

| Target | Measured result | Test |
|---|---:|---|
| Idle memory under 150 MB | 6.89 MiB container memory | `docker stats --no-stream crabgate-phase9-app` after load settled |
| Public Event pages at least 1,000 requests/s | 8,898 requests/s | 4,000 GET `/events/phase9-event` with 50 connections; one-second rendered HTML cache |
| Dynamic requests at least 100 requests/s | 6,351 requests/s | 4,000 GET `/` with 50 connections; uncached Shop index |
| Checkout, zero oversold Tickets | 2 Tickets issued for 20 simultaneous checkout attempts against capacity 2 | `cargo test simultaneous_checkouts_reserve_only_the_last_two_tickets` |

Before caching, the same Event endpoint in the preceding release image served 4,000 valid responses at 3,045 requests/s. Each Event cache entry is scoped to an Organizer and Event, and expires after one second. `Cache-Control: public, max-age=1` applies to the public Event HTML. The checkout transaction still checks current inventory. An Event edit through the Back Office returned 303; the immediate public response held the prior title, and the updated title appeared after 1.01 seconds.

Commands:

```sh
docker build -t crabgate:phase9 .
docker update --cpus=2 crabgate-phase8-app
python3 scripts/load-test.py /events/phase9-event 4000 50 5160
python3 scripts/load-test.py / 4000 50 5160
docker stats --no-stream --format '{{.Name}} {{.MemUsage}} {{.CPUPerc}}' crabgate-phase9-app
cargo test simultaneous_checkouts_reserve_only_the_last_two_tickets
```

The phase 9 container reused the isolated phase 8 container's bind mount and environment, and was started with `--cpus 2 -p 5160:5150`. The load client was first run from `/tmp/crabgate-phase9-load.py`; the identical source is saved as `scripts/load-test.py`. ApacheBench on this host reported 4,000 write errors and transferred zero bytes, so its apparent 27,749 requests/s was discarded. A direct host-side SQLite edit was not visible through an already open connection across the Docker Desktop bind mount; restarting the container made it visible. The cache expiry check therefore used the Back Office editor and the app's own connection.

The final Phase 10 release image, with security middleware enabled, served 4,000 valid cached Event responses at 7,886 requests/s and 4,000 valid uncached Shop responses at 6,334 requests/s under the same 2 vCPU limit. Container memory after the runs was 16 MiB. The final image also returned 404 for an unknown route and missing static asset, and 403 for a cross-origin POST.

## Shop index with 100 Events

Measured on 30 September 2026 on the same Mac, using native debug builds and a local SQLite database. The fixture had 100 future Published Events, two Ticket Types per Event, and no Orders. Each run sent 300 GET `/` requests with 20 persistent connections through `scripts/load-test.py`. All 1,800 responses were HTTP 200 with the same 67,305-byte body. The three runs for each build were sequential, with one warm-up request before each set.

| Build | Requests/s in three runs | Median |
|---|---:|---:|
| `7ef3ce4` before grouped Ticket Type reads | 92.4, 92.0, 92.0 | 92.0 |
| Grouped Ticket Type reads | 279.1, 281.8, 284.2 | 281.8 |

The Shop index now reads Ticket Type prices and capacities in one grouped query instead of one query per Event. The median throughput improved 3.1 times for this fixture. These are native debug-build measurements, so they are separate from the 2 vCPU release-image targets above. The initial comparison reused a stale Cargo executable and was discarded; the changed build was rebuilt before these numbers were recorded. To repeat a larger-fixture check, start the app once against a fresh test database, stop it, run `python3 scripts/seed-load-fixture.py <organizer-db-path> --events 100`, restart it, and run `python3 scripts/load-test.py / 300 20 <port>`.

The current release image was also built with `docker build -t crabgate:audit .` and run with `--cpus=2`. The reusable fixture script seeded 100 future Published Events and 200 Ticket Types into a fresh Organizer database. With 50 persistent connections, three runs of 1,000 GET `/` requests served 566.1, 616.4, and 658.5 requests/s; every response was HTTP 200 with a 67,503-byte body. A 4,000-request cached Event run served 8,072.6 requests/s with valid 6,129-byte bodies. Container memory after load was 17.17 MiB. The larger-fixture Shop result remains above the 100 requests/s dynamic target, and the cached Event result remains above the 1,000 requests/s target. The release-image comparison against `7ef3ce4` was not run, so the 3.1-times improvement above applies only to native debug builds.

## Release binary size

**Measured on 5 October 2026:** Linux/amd64, Rust 1.98, Debian Bookworm. The baseline binary is from `ghcr.io/alplus-tech/crabgate:2e92e288172ee6459236a2ab5776f5060fd6fbe8`. The source for the other build is the same commit. Linux builds ran through Rosetta in the existing Colima VM; stack capture ran through QEMU because Rosetta did not unwind application frames.

All four runtime images use the same fresh Debian base, installed packages, assets, config and entrypoint. MB means 1,000,000 bytes. Unpacked bytes are the sum of decompressed OCI layer archives, including their tar headers. Transfer bytes are the sum of compressed OCI layer descriptors; an update can reuse existing layers. These are controlled comparisons, not production disk allocation or measured download times.

| Measured variant | Binary MB | Unpacked layers MB | Compressed layers MB |
|---|---:|---:|---:|
| Line tables for every crate | 229.6 | 325.6 | 93.1 |
| Every crate, zlib compression | 88.2 | 184.2 | 92.1 |
| Workspace line tables | 95.0 | 191.0 | 62.9 |
| Workspace line tables, zlib compression | 58.2 | 154.3 | 62.4 |

**Measured:** Debug sections account for 181.9 MB of the original binary: a separate stripped control is 47.7 MB. Compression leaves the executable `.text` section byte-for-byte unchanged. The selected combination reduces binary bytes by 74.6%, unpacked layer bytes by 52.6%, and compressed layer bytes by 33.0%. Compression alone changes transfer bytes by only 1.1%.

Release builds keep `line-tables-only` for CrabGate and its migration crate. The [Cargo dependency profile override](https://doc.rust-lang.org/cargo/reference/profiles.html#overrides) disables new debug data for non-workspace crates. The image build applies [GNU objcopy zlib-gABI compression](https://www.sourceware.org/binutils/docs/binutils/objcopy.html) to the remaining debug sections. Symbols and panic unwinding are retained. Dependency function names remain available; generic or inlined code compiled in workspace crates and the prebuilt standard library can still have source lines.

**Measured:** The final Cargo profile builds successfully on Linux and produces the same 58,239,024-byte compressed binary as the comparison. A probe linked against the release Sentry libraries captures the literal file `backtrace_probe.rs` and line `3`, both before and after debug compression. The handler-panic test also requires the source file and exact panic line in the captured event. Format, Clippy, and all 92 Rust tests pass. The selected runtime image serves the tool pages, scripts and QR route in an isolated test container.

**Inferred:** Fewer compressed layer bytes should reduce pull time on the same connection. Download time, production disk allocation, and memory after symbolication were not benchmarked. Compression can allocate memory when a stack is symbolicated, so the measurements do not establish zero memory impact.

Raw build, section, image and stack-capture logs are in `/tmp/crabgate-binary-size/`. The comparison binaries and container context are ignored under `data/binary-size/`.

## Release build cache

**Measured on 5 October 2026:** Linux/amd64 builds through Rosetta in Colima, with 2 CPUs and 4 GiB RAM. Both commands used `docker build --platform linux/amd64 --progress plain`. The cold run had no compiled dependency layer for the new Dockerfile; existing base and system-package layers were cached. Native Rust checks also ran during the beginning of the cold run. The source-change run used an isolated copy of the same source with a literal comment added to the Rust-generated robots response. These are single local observations, not native GitHub runner benchmarks.

| Measured build | Total | Dependency compile | Real workspace compile |
|---|---:|---:|---:|
| Cold dependency layer | 30m55s | 24m44s | 5m45s |
| Rust source change, warm dependency layer | 8m20s | Cached | 8m01s |

**Measured:** The source-change run took 73.1% less elapsed time. Its Cargo log contains only `migration` and `crabgate` compilation. Workspace compile time varied between the two runs; the measurements do not establish a stable compile-time estimate. The original runtime image served the original robots response, and the rebuilt image served the exact changed response. Both images passed nine tool-page checks, five script checks, the tools index, sitemap metadata, and valid/overlong QR checks. The cold binary remained 58,239,024 bytes. Format, Clippy, all 92 Rust tests, and a workflow gate assertion passed.

The dependency stage is keyed by both Cargo manifests, the lockfile and the build base. It compiles placeholder workspace source, then uses [Cargo package cleaning](https://doc.rust-lang.org/cargo/commands/cargo-clean.html) to remove workspace artifacts before real source is copied. The existing `type=gha,mode=max` export preserves this regular Docker layer; this does not rely on cache mounts. Checks and image creation run in parallel, and deployment requires both to succeed.

**Measured baseline:** The preceding native GitHub deployment took 19m47s: checks 3m15s, image job 15m47s, deployment 34s, plus scheduling overhead. Its Cargo release step took 12m25s and cache export took about 2m18s. **Inferred:** Parallel jobs remove the serial checks wait, and a warm dependency layer avoids recompiling third-party crates after source changes. The first deployment must fill the new cache. Future native source-change deployment time is not established by these local timings.

Raw logs, timing JSON, isolated source and HTTP evidence are in `/tmp/crabgate-build-cache/`.

## Runtime asset changes and compiler experiment

**Measured on 5 October 2026:** the same Linux/amd64 Colima/Rosetta environment, 2 CPUs and 4 GiB RAM. The Docker compile stage now copies only the embedded 404 page, two icons and fee-rate JSON. All templates, CSS, JavaScript and config are copied into the runtime image after compilation.

**Measured:** A controlled rebuild in the same isolated source context changed the navigation CSS gap and the sign-in template. It finished in 2.38 seconds, with the complete Rust build layer cached. A preceding build of that context compiled the workspace in a total image-build time of 4m26s. The final repository image rebuilt in 4.52 seconds after template edits, also with Rust cached. SHA-256 checks prove identical binary bytes in both before/after pairs. The final image passed tool-page, script, sitemap, QR and sign-in flow checks. An earlier cross-context attempt unexpectedly invalidated `COPY src`; that attempt is not evidence of an asset-only cache hit. Raw logs preserve all attempts.

A separate build used Cargo's package override to test `opt-level=2` for CrabGate alone, keeping dependency optimisation and source-line settings unchanged. The default and candidate builds used identical Rust source. Each compile timing is a single observation through emulation; a separate default build took 8m49s with native checks running during its start, so the timing spread is larger than the candidate's observed gain.

| Measured variant | Image build | Binary bytes | Median Shop requests/s |
|---|---:|---:|---:|
| Default `opt-level=3` | 266.27s | 58,334,192 | 505.55 |
| Candidate `opt-level=2` | 217.88s | 58,026,672 | 462.95 |

**Measured:** Request tests used one shared isolated database with 100 future Published Events and 200 Ticket Types, no Orders, and a 2 CPU limit. The two variants ran in alternating blocks of three runs, producing six runs of 1,000 uncached Shop index requests per variant with 50 persistent connections. All 12,000 measured responses were HTTP 200 with a 67,309-byte body. Each block first passed a 200-request warm-up. The candidate's observed build time was 18.2% shorter, its median throughput was 8.4% lower, and its binary was 0.5% smaller. **Inferred:** This experiment does not justify reducing optimisation, so production retains the default level. The observations do not establish the same trade-off on a native runner or another workload.

**Measured:** The marketing navbar and shop-address sign-in page were inspected at 360 and 1280 CSS pixels. The Sign in button has a 44px target and a 3px keyboard focus ring. Browser checks cover navigation, empty-input validation, keyboard submission and the unknown-shop error, with no overflow or unexpected console/CSP errors. The expected HTTP 400 for the error state is recorded. Format, Clippy and all 93 Rust tests pass; request tests also cover an existing shop's redirect and email form, malformed and overlong input, marketing-host isolation and the local-edition redirect. The independent scoped design review returned `ship`, and the documentation check confirmed that DESIGN.md needs no changes.

Build logs, timing JSON, request benchmark results, screenshots and browser evidence are in `/tmp/crabgate-runtime-assets/`.
