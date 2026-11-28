# Android counterparts of the iOS memory tools

What an Android emulator offers over adb for `mem_leaks`, `mem_heap`, `mem_vmmap` and `mem_history`
(the iOS methods that run the host's `leaks`, `heap`, `vmmap --summary` and `malloc_history` on one
app), what each mechanism needs, and an API for the ones that can match the iOS result shapes.
Issue [#415](https://github.com/kaeawc/simee/issues/415), part of the Android/iOS parity epic
[#422](https://github.com/kaeawc/simee/issues/422). Heap dumps themselves are
[#167](https://github.com/kaeawc/simee/issues/167)'s.

- Written 2026-11-28 from the sources linked below and from simee's own code (`crates/sim-api/src/memtools.rs`,
  `crates/sim-android/src/procs.rs`). **No emulator or `adb` was available while writing.** Nothing here
  was run on a device, so there are no measured timings or output sizes: every number is from a
  cited source, and every behaviour the sources don't settle is listed under
  [What needs a device](#what-needs-a-device).
- Each claim is tagged **[sourced]** (stated by the linked document or source file) or **[inferred]**
  (follows from sources, not stated by them).

## Summary

| iOS method | Android counterpart | Verdict |
| --- | --- | --- |
| `mem_vmmap` | `/proc/<pid>/smaps`, grouped by mapping kind | Build. Same shape, minus iOS-only columns. Needs root (`su 0`) on most images |
| `mem_heap` | An `am dumpheap` hprof for the class histogram; `dumpsys meminfo [-d] <pkg>` for the breakdown, as a separate Android-only `mem_meminfo` | Histogram: build (hprof reader, no new dependency). Breakdown: build first (any app, no root) |
| `mem_leaks` | `dumpsys meminfo --unreachable <pkg>` (libmemunreachable) | Build, narrower than iOS: debuggable app, leaks as address and size, stacks only with `malloc_debug` |
| `mem_history` | `malloc_debug` `backtrace` dump | Not a reliable equivalent. Stays iOS-only for now; a design is sketched so the decision can be revisited |

## 1. The mechanisms

### `dumpsys meminfo <pkg|pid>` and `-d`

- **What it gives.** PSS, private clean/dirty, swapped dirty and heap size/alloc/free for categories
  (Native Heap, Dalvik Heap, Dalvik Other, Stack, `.so`/`.apk`/`.dex`/`.oat`/`.art` mmap, Unknown,
  graphics rows, TOTAL), then an Objects block (`Views`, `ViewRootImpl`, `AppContexts`,
  `Activities`, `Local Binders`, …). `-d` adds the Dalvik details (`.Heap`, `.LOS`, `.GC`,
  `.JITCache`, `.Zygote`, `.NonMoving`, `.IndirectRef`). **[sourced]**
  ([dumpsys](https://developer.android.com/studio/command-line/dumpsys), "View memory allocations")
- **Needs.** It runs in `system_server` and the `dumpsys` caller needs only the shell user: no root,
  no debuggable flag **[inferred: the docs show it on arbitrary packages such as Maps and Gmail, and
  procstats/meminfo are what `adb shell` developers run on release apps]**. Available on every API
  level simee provisions; the column set varies by release (the docs warn the output "might vary
  slightly"), so the parser must key on row names, not column positions.
- **Time and size.** `dumpsys` has a 10 s default timeout (`-t`). **[sourced]** Real durations and
  sizes: needs a device. The report is a few dozen lines **[inferred from the samples]**.
- **Parse without a new dependency.** Yes: fixed-width text, rows named by category. The
  `--checkin` flag gives a machine-friendly form for some services (the page documents it for
  `batterystats`); whether `meminfo --checkin` is stable enough to prefer over the table is a
  device question.
- **Matches iOS tool:** `vmmap --summary` (regions) and the totals half of `heap`.

### `am dumpheap [-n] [-g] <process> <file>` and hprof

- **What it gives.** A Java heap dump in the file you name (default
  `/data/local/tmp/heapdump-<time>.prof`). `-n` dumps the native heap instead of the managed heap,
  `-g` runs a GC first, `-m` adds malloc info, `-b [format]` dumps bitmaps. **[sourced]**
  ([`ActivityManagerShellCommand.java`](https://android.googlesource.com/platform/frameworks/base/+/main/services/core/java/com/android/server/am/ActivityManagerShellCommand.java),
  `runDumpHeap`.) The shell command itself performs no permission check; the check is in the
  service that receives the call, which I could not read (the source page truncated before it).
  **[Unverified: whether a non-debuggable, non-profileable app is refused; Android Studio's own
  docs say heap dumps need a "debuggable" run for complete data.]**
  ([memory profiler](https://developer.android.com/studio/profile/memory-profiler))
- **hprof format.** ART writes `JAVA PROFILE 1.0.3` with an identifier size of 4, STRING,
  LOAD_CLASS, STACK_*, HEAP_DUMP_SEGMENT and HEAP_DUMP_END records, `CLASS_DUMP`,
  `INSTANCE_DUMP`, `OBJECT_ARRAY_DUMP`, `PRIMITIVE_ARRAY_DUMP` sub-records, and the Android-only
  `HEAP_DUMP_INFO` (0xFE) that switches between the `app`, `zygote` and `image` heaps. **[sourced]**
  ([`hprof.cc`](https://android.googlesource.com/platform/art/+/main/runtime/hprof/hprof.cc))
  Android's file isn't plain Java SE hprof: `hprof-conv` converts it for Java tools.
  **[sourced]** (memory profiler page)
- **Cost on the app.** ART holds a GC critical section and suspends all threads for the whole dump
  (`ScopedSuspendAll`). **[sourced]** (`hprof.cc`) The app is frozen for as long as the dump takes,
  so the method must say so.
- **Native sizes.** ART's file does not carry native allocations; class overhead and string
  characters appear as synthetic arrays, and the source's comments say IMT/ArtField/ArtMethod
  overhead "may be shown in the future". **[sourced]** (`hprof.cc`) Bitmap pixels are in Java
  memory only on old releases; Studio's "Native Size" column needs Android 7.0+ **[sourced]** and
  comes from a separate mechanism simee would not have.
- **Time and size.** Needs a device; a managed heap dump is the size of the live heap
  **[inferred]**, tens of MB for a normal app.
- **Parse without a new dependency.** A class histogram (count and shallow bytes per class) needs
  only the string table, `LOAD_CLASS`, and a linear pass over instance, object-array and
  primitive-array records: a streaming reader of a few hundred lines, no heap graph. **[inferred]**
  Retained size, dominators and leak paths need the full graph and are LeakCanary-class work (see
  below).
- **Matches iOS tool:** `heap` (the class table: `class`, `count`, `bytes`, `avg_bytes`).

### `/proc/<pid>/smaps`, `smaps_rollup` and `showmap`

- **What it gives.** Per mapping: `Size`, `Rss`, `Pss`, `Shared_/Private_` `Clean/Dirty`,
  `Referenced`, `Anonymous`, `Swap`, `SwapPss`, `Locked`, `VmFlags`, after a header line in
  `/proc/<pid>/maps` format. `smaps_rollup` is the same sums for the whole process plus `Pss_Anon`,
  `Pss_File` and `Pss_Shmem`. **[sourced]**
  ([kernel `proc.html`](https://docs.kernel.org/filesystems/proc.html))
- **Needs.** Reading another process's files needs `PTRACE_MODE_READ` access (`CAP_SYS_PTRACE`) or
  `CAP_PERFMON`. **[sourced]** (same page) So the shell user can't read an app's `smaps`; root can
  (`su 0` exists on `google_apis` images, which simee already relies on for `kill`:
  `crates/sim-android/src/procs.rs`). `google_apis_playstore` images have no `su`: **[inferred from
  the same code and `docs/android.md`; confirm on a device]**. For a debuggable app, `run-as <pkg>`
  runs as the app's uid **[inferred]**; whether SELinux lets that domain read its own `/proc/<pid>`
  files is a device question.
- **`showmap`.** AOSP builds `showmap` in `system/memory/libmeminfo/tools`, with no eng/userdebug
  restriction in its `Android.bp` (also `procrank`, `librank`, `wsstop`). **[sourced]**
  ([`tools/Android.bp`](https://android.googlesource.com/platform/system/memory/libmeminfo/+/main/tools/Android.bp))
  Whether it is installed on production images: needs a device. It aggregates smaps by name; simee
  can do that itself from `smaps`, and reading `smaps` directly doesn't depend on the binary being
  present, so `showmap` is not needed.
- **Time and size.** `smaps` has one 20-odd-line block per mapping, a few thousand mappings for a
  Java app **[inferred]**; `smaps_rollup` is one block and the docs call it "faster and more
  convenient than summing smaps". **[sourced]** Real numbers: needs a device.
- **Parse without a new dependency.** Yes: `key: value kB` lines. Pure logic, so it goes in `sim-core` with unit
  tests (AGENTS.md).
- **Matches iOS tool:** `vmmap --summary` (`regions`, `total`).

### heapprofd and Perfetto

- **What it gives.** Sampled native allocations with call stacks, as a Perfetto trace
  (flamegraphs in the UI; `trace_processor convert profile` makes pprof). Java allocations with
  `heaps: "com.android.art"`. **[sourced]**
  ([native heap profiler](https://perfetto.dev/docs/data-sources/native-heap-profiler))
- **Needs.** Android 10+ for native, 12+ for Java. On a `user` build only debuggable or
  `profileable` apps; on a `userdebug` build nearly everything except a few critical services.
  **[sourced]** (same page) `<profileable android:shell="true"/>` appeared in API 29; `android:enabled`
  in API 30. **[sourced]**
  ([`<profileable>`](https://developer.android.com/guide/topics/manifest/profileable-element))
  Config fields: `sampling_interval_bytes` (default 4096; 0 is invalid), `process_cmdline`, `pid`,
  `heaps`, `shmem_size_bytes` (default 8 MiB, cap 500 MiB), `block_client`, `dump_at_max` (Android
  11+), `continuous_dump_config`. **[sourced]**
  ([trace config](https://perfetto.dev/docs/reference/trace-config-proto))
- **Time and size.** A recording runs for as long as the caller asks (seconds to minutes); the
  trace size grows with the allocation rate. The docs give no overhead number: a larger interval
  trades accuracy for load. **[sourced]** Measured values: needs a device.
- **Parse without a new dependency.** No. The output is a protobuf trace: reading allocation
  stacks out of it means a protobuf decoder and Perfetto's profile schema (or shipping
  `trace_processor`). simee should record and pull the trace (that is #167's Perfetto item) and
  leave analysis to `ui.perfetto.dev`, not turn it into `mem_*` result fields.
- **Matches iOS tool:** `mem_leaks`' stacks and `mem_history`'s stacks, as profiles of live
  allocations; not an address lookup, and not a leak list.

### libmemunreachable: `dumpsys meminfo --unreachable`

- **What it gives.** A mark-and-sweep over native memory: allocations no root (globals, registers,
  stacks) reaches. The report is a summary (`N bytes in M unreachable allocations`), then per leak
  `N bytes unreachable at <hex address>`, an approximate upper-bound size, optionally the first
  bytes of contents, and "similar" groups. Backtraces appear only when `malloc_debug` backtraces are
  on. The marking is imprecise. **[sourced]**
  ([README](https://android.googlesource.com/platform/system/memory/libmemunreachable/+/main/README.md),
  [`MemUnreachable.cpp`](https://android.googlesource.com/platform/system/memory/libmemunreachable/+/main/MemUnreachable.cpp):
  the strings `bytes unreachable at`, `unreachable allocation`, `similar unreachable bytes`,
  `contents:`.)
- **Invocation.** `dumpsys -t 600 meminfo --unreachable [process]`; zygote loads the library.
  **[sourced]** (README)
- **Needs.** The README names no root requirement for the default mode; backtraces need
  `adb root` and `setprop` on a userdebug device, then an app restart. **[sourced]** A secondary
  source says the option works only for `FLAG_DEBUGGABLE` apps **[unverified: a machine-translated
  blog; find the check in `ActivityManagerService`/`MemBinder` or test on a device]**. The target is
  briefly paused while its threads stop. **[sourced]**
- **Time and size.** The default `dumpsys` timeout is shorter than a scan needs; the README says
  `-t 600`. **[sourced]** A real scan's duration and the report size scale with the heap and the
  leak count: needs a device. The library's `limit` argument truncates the sorted leak list.
  **[sourced]** (`MemUnreachable.cpp`)
- **Parse without a new dependency.** Yes. The format is line-oriented; the blog's regex
  `[0-9]+ bytes unreachable at [A-Za-z0-9]+` is a fair anchor but simee should parse from a
  recorded fixture, not from a description.
- **Matches iOS tool:** `leaks`. Differences: no type names (native memory has none), no
  zone, and stacks only if malloc_debug backtraces were enabled before the app started.

### `malloc_debug` backtraces

- **What it gives.** `backtrace[=MAX_FRAMES]` (default 16, max 256) records the stack of every
  allocation. Signal `SIGRTMAX-17` (47) writes "Android Native Heap Dump" to
  `/data/local/tmp/backtrace_heap.<pid>.txt` (Android P+): one line per record with a zygote-child
  flag `z`, `sz`, `num` and the frame addresses `bt`, then a `MAPS` section (`/proc/<pid>/maps`) to
  decode them. `record_allocs` logs every allocation and free (dumped on `SIGRTMAX-18`);
  `leak_track` dumps live allocations at exit; `check_unreachable_on_signal` (API 34, signal 48)
  runs libmemunreachable on a signal. **[sourced]**
  ([README](https://android.googlesource.com/platform/bionic/+/main/libc/malloc_debug/README.md))
- **Needs.** On a rooted device, `setprop wrap.<pkg> '"LIBC_DEBUG_MALLOC_OPTIONS=backtrace logwrapper"'`
  then `am force-stop <pkg>` (API 26+); verify with `malloc debug enabled` in logcat. Android 12
  has a bug that can break `wrap.<pkg>`; the documented fix is
  `dalvik.vm.force-java-zygote-fork-loop` set to `true`, then a restart. Off a rooted device the page
  points to the NDK's `wrap.sh`, which needs an app built for it. **[sourced]** (same README) The
  app must be restarted, so this is a launch option, like iOS's `malloc_stack_logging`.
- **Cost.** "Slows allocation significantly"; libmemunreachable's README says backtracing every
  allocation can slow an app enough to prevent startup or hide leaks and recommends
  `backtrace_size`, `backtrace_min_size` and `backtrace_max_size`. **[sourced]**
- **Output.** Frames are raw addresses plus a maps table. Symbols need the app's unstripped
  libraries on the host, or an `addr2line`/`llvm-symbolizer` run: the dump has no symbol names.
  `backtrace_full` (Q+) adds unwinding info. **[sourced]**
- **Parse without a new dependency.** The text format is trivial to read. Symbolizing needs
  `llvm-symbolizer` (an NDK tool, not a Rust dependency) and the unstripped `.so` files, which
  simee has for an app built from the host and does not have for a prebuilt APK.
- **Matches iOS tool:** `mem_leaks`' stacks (allocation site totals) and, loosely, `mem_history`.

### LeakCanary-style hprof analysis

LeakCanary finds retained `Activity`, `Fragment`, `View` and `ViewModel` instances from an hprof by
building the object graph and tracing shortest paths from GC roots. Android Studio integrates it
as a task, moving the analysis to the dev machine. **[sourced]**
([memory profiler](https://developer.android.com/studio/profile/memory-profiler), "LeakCanary in
Android Studio Profiler".) simee has no graph library and the work (dominators, GC-root paths,
the platform's leak-pattern knowledge) is far past "small PR". Recommendation: out of scope for
`mem_*`. A cheap proxy is in `dumpsys meminfo`: the `Activities`, `AppContexts`, `ViewRootImpl` and
`Views` counts, which Android's docs call a way to "quickly identify leaked `Activity` objects".
**[sourced]** (dumpsys page)

## 2. What works on which image

| Mechanism | `google_apis_playstore` (user build, no `su`) | `google_apis` (root: `su 0`) | `default` AOSP (userdebug-like) |
| --- | --- | --- | --- |
| `dumpsys meminfo <pkg>` and `-d` | Yes **[inferred]** | Yes | Yes |
| `/proc/<pid>/smaps` | Not for another uid; `run-as` for a debuggable app (device check) | Yes | Yes |
| `am dumpheap` managed | Debuggable app (profileable: device check) | Yes | Yes |
| `dumpsys meminfo --unreachable` | Debuggable app **[unverified]** | Yes | Yes |
| heapprofd | Debuggable or profileable app, API 29+ | Nearly any process | Nearly any process |
| `malloc_debug` `wrap.<pkg>` | No (needs `setprop wrap.*`, root, or `wrap.sh` in the APK) | Yes | Yes |

The `su 0` column rests on `procs.rs`'s comment that google_apis images have it; the other two
columns are inferred from the image tags and the sources' build-type language, and the table's
cells marked "device check" are open questions below. A tool that isn't allowed on an image should
say which image or app flag it needs, as `kill` does for root today.

## 3. Proposed API

All four keep their iOS parameters: `device, bundle_id | pid, raw?, timeout_s?(120)`. On Android
`bundle_id` is the package name; `pid` must be one of the device's processes (as `processes`
reports); `raw: true` adds the source report as printed; every answer has `pid` and `warnings`,
and the platform column in `help` moves from `iOS` to `Android, iOS` for the methods built.
Each backend runs over adb with the existing deadline and quoting helpers in `sim-android`; no new
dependencies.

### `mem_vmmap` (build)

Source: `su 0 cat /proc/<pid>/smaps` (fallback `run-as` for a debuggable app, to be confirmed).
Answer, matching `outputs.rs`'s `{...@mem, regions: [object], total: object|null, ...}`:

- `regions`: one row per mapping kind, `{region, virtual_bytes, resident_bytes, dirty_bytes,
  swapped_bytes, region_count}` plus the Android-only `pss_bytes`, `private_dirty_bytes` and
  `private_clean_bytes`. `virtual` is `Size`, `resident` is `Rss`, `dirty` is `Private_Dirty +
  Shared_Dirty`, `swapped` is `Swap`, `region_count` is the number of mappings. Kinds: `[heap]`,
  `[stack]`, `[anon:libc_malloc]` and `[anon:scudo:*]` (native heap), `[anon:dalvik-*]` (ART), file
  mappings by extension (`.so`, `.apk`, `.dex`/`.vdex`/`.odex`, `.oat`, `.art`, `.ttf`), `/dev/*`
  (graphics), memfd/ashmem and `[anon:*]` others. The names come from the mapping's pathname; the
  kind list is a first cut to settle against real `smaps` fixtures, not a claim about how
  `dumpsys meminfo` buckets them (I could not read that code).
- `total`: the sums, same keys.
- `volatile_bytes`, `nonvol_bytes` and `empty_bytes` (iOS purgeable-memory columns) are not
  filled: Linux has `LazyFree` but not the same meaning. Omit the keys rather than send zeros.
- `readonly_libraries`, `writable_regions`, `malloc_zones`, `malloc_total`: omitted (optional in
  the iOS schema). Android's allocators have no zone table.
- Extra: `rollup` `{pss_anon_bytes, pss_file_bytes, pss_shmem_bytes, swap_pss_bytes}` from
  `smaps_rollup` when the kernel has it.
- Failure text: when the shell can't read the file, say which image or flag would allow it,
  not just "permission denied".

### `mem_heap` (build, after the hprof reader)

The `dumpsys meminfo` breakdown does not fit `mem_heap`'s shape: its rows are categories (Native
Heap, Dalvik Heap, `.so` mmap), not classes, and the iOS parser treats an empty `classes` table as
an error. So the breakdown ships as its own Android-only method, `mem_meminfo` (below), and
`mem_heap` is the class histogram only. Don't put categories in `classes`.

Class histogram: `am dumpheap [-g] <pkg> /data/local/tmp/simee-<pid>.hprof`, `adb pull`,
delete the device copy (as #167 plans), parse. `classes`: `{class, count, bytes, avg_bytes}` with
`bytes` the shallow size, most instances first (`heap`'s order). `nodes` is the instance count,
`bytes` the total shallow size, `zones` the number of heaps in the file (`app`, `zygote`,
`image`; `heap` field per row would be an extension). `raw` is not offered (an hprof is binary):
say so in `help`. Parameter `gc?`(true) maps to `-g`. The app is frozen during the dump (`hprof.cc`).
The class histogram shares its parser with #167's heap dump, so the two issues ship one hprof
reader and `mem_heap` uses it.

### `mem_meminfo` (Android-only, build first)

`{...@mem, total_pss_bytes, categories: [{name, pss_bytes, private_dirty_bytes, private_clean_bytes,
swapped_dirty_bytes, heap_size_bytes?, heap_alloc_bytes?, heap_free_bytes?}], dalvik_details?,
objects: {views, view_root_impl, app_contexts, activities, ...}}`. It is the "memory" row of #167;
name it so #167 and this issue don't both add it. If #167 lands it first as e.g. `memory`, reuse
that and drop this item. The iOS counterpart of this is the `physical_footprint_bytes` header the
iOS tools already answer.

### `mem_leaks` (build, narrower)

Source: `dumpsys -t <timeout_s> meminfo --unreachable <pkg>`. Answer matching `{...@mem, leak_count,
stacks, leaks: [object], leaked_bytes?, note?}`:

- `leak_count`, `leaked_bytes` from the summary line.
- `leaks`: grouped like iOS (`count`, `bytes`, `instances`, `stack`); each instance
  `{address, size}` (no `zone`); the group key is the allocation stack when present and the "similar"
  grouping libmemunreachable prints otherwise; no `type`, since native blocks have none.
- `stacks`: true only when backtraces are in the report. Frames `{address, image, symbol?, source?}`
  as `@frame`, innermost first; `symbol` unset without a symbolizer. Same `note` as iOS when
  `count > 0 && !stacks`, naming the launch option.
- Launch parameter: `launch {malloc_stack_logging: true}` on Android sets
  `wrap.<pkg>` with `backtrace` (and `libc.debug.malloc.options` where `wrap` is broken) before
  `am start -S`, and `launch`'s docs row says so. This needs root; without it the launch fails
  with the reason, not silently without stacks.
- `nodes` and `malloced_bytes` (iOS totals): omitted. The log line libmemunreachable writes
  (`N bytes in M allocations unreachable out of X bytes in Y allocations`) goes to logcat, so a
  later pass could read it for these.

### `mem_history` (stays iOS-only for now)

`mem_history {address}` asks "who allocated this address, as events". `malloc_debug`'s backtrace
dump has no per-address query: it's an aggregate by stack (`sz`, `num`, `bt`), with no address and
no frees. `record_allocs` has addresses, but it is a binary-ish allocation trace meant for the
benchmark replayer, at up to 50 million records, and slows the app.
**[sourced]** (malloc_debug README) A reliable `mem_history` needs a different method, "allocations
by stack", not an address lookup. Say so in the API error:
`mem_history is not available on android devices: malloc_debug records allocation sites, not
addresses; use mem_leaks with stacks`, and keep it iOS-only. Revisit if a device shows `record_allocs`
is usable on an emulator app within the default timeout. This is the only call of the four where
the research answer is "no equivalent that is reliable", which is what #415 asked us to say plainly.

### Not in `mem_*`

- Perfetto/heapprofd recording (profiles, flamegraphs): #167's trace method, with heapprofd as one
  of its data sources. Analysis stays in `ui.perfetto.dev` (protobuf, above).
- Retained size and leak paths (LeakCanary): out of scope.

## 4. Order of work

Each item is one PR, with parsers tested against **recorded** device output as fixtures (AGENTS.md:
fixtures are captured, never written from a description) and the method tested against a fake adb
(`sim-android`'s `testing.rs`), as the issue's "done when" asks.

1. **Capture fixtures** (needs a device): `smaps`, `smaps_rollup`, `dumpsys meminfo -d`,
   `--unreachable` (with and without backtraces) and a small `am dumpheap` from a test app, on
   `google_apis` API 34 and 36, committed under `crates/sim-api/tests/fixtures/memtools/android/`.
   Everything below depends on these. Ship with 2 if no device is free.
2. **`mem_vmmap`** on Android: smaps parser and grouping in `sim-core` (pure), the adb call in
   `sim-android`, the dispatch in `sim-api`'s `android.rs`, platform columns, `docs/api.md`,
   `docs/mcp.md`, schema types. Needs `su 0`; error text for the rest.
3. **`mem_meminfo`** (or #167's `memory`, whichever lands first): `dumpsys meminfo -d` parser.
4. **`mem_leaks`**: the `--unreachable` parser and method with no stacks, `note` as for iOS.
5. **`launch` `malloc_stack_logging` on Android**: `wrap.<pkg>`, restart, and verification via
   logcat's `malloc debug enabled`; then `mem_leaks` stacks and the frames' symbolization
   (`llvm-symbolizer` if found, raw addresses otherwise).
6. **hprof reader and `mem_heap`** (with #167's heap dump: whichever lands first owns the parser):
   streaming class histogram, no graph.
7. **`mem_history`'s Android answer**: the explanatory error and the `help` platform text,
   no behaviour change. Can ship with 2.

## What needs a device

Run on an emu-booted AVD (per the owner's 2026-11-28 rule, these merge on harness and CI evidence
and are confirmed on main):

- Whether `dumpsys meminfo <pkg>` works for a non-debuggable app from the shell user, on
  `google_apis_playstore`; whether `--checkin` is usable for `meminfo`.
- Whether `su 0` is present on each image type simee provisions, and the error `su` gives without it.
- Whether `run-as <pkg> cat /proc/<pid>/smaps` is allowed for a debuggable app under SELinux.
- Whether `showmap` is installed on any image (not needed, but cheap to know).
- Whether `dumpsys meminfo --unreachable` is refused for a non-debuggable app, its time on a small
  and a large heap, its effect on the emulator app (the target pauses), and its output with and
  without `wrap.<pkg>` backtraces.
- Whether `wrap.<pkg>` works on API 34–36 images (the README flags an Android 12 bug), and the
  slowdown of `backtrace` on the emulator's app.
- Whether `am dumpheap` is refused for a non-debuggable, non-profileable app; the time and size of the
  dump for a test app, and how long the app is frozen.
- Time and output size for each call, so `docs/api.md`'s Memory tools section can carry Android
  costs the way it carries iOS's, and `docs/benchmarks.md` a row (AGENTS.md: no published number
  without a measured row).
- Whether `mem_history` has a usable shape on `record_allocs`.

## Sources

- dumpsys: <https://developer.android.com/studio/command-line/dumpsys>
- Memory profiler and heap dumps: <https://developer.android.com/studio/profile/memory-profiler>
- `<profileable>`: <https://developer.android.com/guide/topics/manifest/profileable-element>
- Perfetto native heap profiler: <https://perfetto.dev/docs/data-sources/native-heap-profiler>
- Perfetto trace config (`HeapprofdConfig`): <https://perfetto.dev/docs/reference/trace-config-proto>
- malloc_debug: <https://android.googlesource.com/platform/bionic/+/main/libc/malloc_debug/README.md>
- Bionic native allocator (`malloc_iterate`, libmemunreachable): <https://android.googlesource.com/platform/bionic/+/main/docs/native_allocator.md>
- libmemunreachable: <https://android.googlesource.com/platform/system/memory/libmemunreachable/+/main/README.md>
  and `MemUnreachable.cpp` beside it
- `am dumpheap`: <https://android.googlesource.com/platform/frameworks/base/+/main/services/core/java/com/android/server/am/ActivityManagerShellCommand.java>
- ART hprof writer: <https://android.googlesource.com/platform/art/+/main/runtime/hprof/hprof.cc>
- libmeminfo tools (`showmap`, `procrank`): <https://android.googlesource.com/platform/system/memory/libmeminfo/+/main/tools/Android.bp>
- Linux `/proc/<pid>/smaps`, `smaps_rollup`: <https://docs.kernel.org/filesystems/proc.html>
- simee: `docs/api.md` "Memory tools", `crates/sim-api/src/memtools.rs`, `crates/sim-api/src/outputs.rs`,
  `crates/sim-android/src/procs.rs`.
