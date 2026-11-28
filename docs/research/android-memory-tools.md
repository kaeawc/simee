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
| `mem_leaks` | `dumpsys meminfo --unreachable <pkg>` (libmemunreachable) | Build, narrower than iOS: debuggable or shell-profileable apps only (**inferred**), at most 100 leaks, a 30 s scan cap, leaks as address and size, stacks only with `malloc_debug` |
| `mem_history` | `malloc_debug` `backtrace` dump or `record_allocs` | Not a reliable equivalent: no one mode has both an address and a stack. Stays iOS-only for now; a design is sketched so the decision can be revisited |

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
  `runDumpHeap`.) The shell command checks nothing; the service does.
  `ActivityManagerService.dumpHeap` requires `SET_ACTIVITY_WATCHER` (the shell has it) and then
  calls `enforceDebuggable(proc)`, which throws `SecurityException("Process not debuggable: <pkg>")`
  unless `Build.IS_DEBUGGABLE` (`ro.debuggable=1`, a userdebug or eng build) or the app has
  `FLAG_DEBUGGABLE`. **[sourced]**
  ([`ActivityManagerService.java`](https://android.googlesource.com/platform/frameworks/base/+/main/services/core/java/com/android/server/am/ActivityManagerService.java),
  `dumpHeap` and `enforceDebuggable`.) A `profileable` app is not enough: on a user build only a
  debuggable app can be dumped, on a userdebug build any app can. Android Studio's docs likewise
  say to run "as debuggable (complete data)".
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
  `crates/sim-android/src/procs.rs`). What decides it is the build's `ro.debuggable`, not `su`
  itself: `su` is present on debuggable (userdebug/eng) builds, and the same `ro.debuggable` is
  what makes AMS accept `am dumpheap` for any app (`Build.IS_DEBUGGABLE`, above). Whether a
  `google_apis_playstore` image is a user build with `ro.debuggable=0`: **[inferred from the same
  code and `docs/android.md`; confirm with `getprop ro.debuggable` on a device]**. For a debuggable app, `run-as <pkg>`
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
  Config fields: `sampling_interval_bytes` (default 4096 bytes per the [native heap profiler page](https://perfetto.dev/docs/data-sources/native-heap-profiler); the proto says it must be set non-zero, and 0 crashes the target before Android 12), `process_cmdline`, `pid`,
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
- **Needs.** Two separate gates, one in each direction.
  - *The report's contents.* `ActivityThread.dumpMemInfo` passes `showContents = appInfo.flags &
    FLAG_DEBUGGABLE || Build.IS_DEBUGGABLE` to `Debug.getUnreachableMemory(100, showContents)`, so
    only a debuggable app (or a debuggable build) gets the hex dump of each leak's bytes.
    **[sourced]**
    ([`ActivityThread.java`](https://android.googlesource.com/platform/frameworks/base/+/main/core/java/android/app/ActivityThread.java),
    `dumpMemInfo`.)
  - *Whether the scan runs at all.* libmemunreachable ptraces the app's threads
    (`MemUnreachable.cpp`: "ptrace all the threads"), and zygote leaves an app process
    non-dumpable unless it is debuggable (`DEBUG_ENABLE_PTRACE`, which calls `EnableDebugger()`)
    or profileable from shell (`PROFILE_FROM_SHELL`, which sets `PR_SET_DUMPABLE` to 1); a
    non-dumpable process refuses ptrace. **[sourced: the zygote and libmemunreachable code;
    inferred: the conclusion that the kernel then refuses the scan's ptrace, so a release app
    that is neither prints `Failed to get unreachable memory`, the failure string in
    `GetUnreachableMemoryString`. Not run; needs a device.]**
    ([`com_android_internal_os_Zygote.cpp`](https://android.googlesource.com/platform/frameworks/base/+/main/core/jni/com_android_internal_os_Zygote.cpp).)
    Those flags come from the app's own manifest: `ProcessList.startProcess` sets
    `DEBUG_ENABLE_PTRACE` from `FLAG_DEBUGGABLE` and `PROFILE_FROM_SHELL` from
    `isProfileableByShell()`, not from `Build.IS_DEBUGGABLE`, so a release app on a userdebug image
    is expected to fail too. **[sourced:
    [`ProcessList.java`](https://android.googlesource.com/platform/frameworks/base/+/main/services/core/java/com/android/server/am/ProcessList.java);
    inferred: that nothing else in the zygote path makes it dumpable on a userdebug build.]**
  - Backtraces need `adb root` and `setprop` on a userdebug device, then an app
  restart. **[sourced]** (README) The target is briefly paused while its threads stop. **[sourced]**
- **Time and size: two caps.** AMS waits for the app's reply on a `TransferPipe` for **30 000 ms**
  when `--unreachable` is set (5 000 ms otherwise): `tp.go(fd, opts.dumpUnreachable ? 30000 : 5000)`.
  **[sourced]** (`ActivityManagerService.java`, `dumpApplicationMemoryUsage`.) That is a fixed
  server-side wait, so `dumpsys -t 600` (the README's advice) raises only the client's timeout and
  can't extend it; a scan that takes longer prints `Got IoException!` instead of a report. And the
  app is asked for at most **100** leaks: `Debug.getUnreachableMemory(100, …)` is hard-coded, and
  libmemunreachable truncates the sorted list to that `limit` (`leaks.resize(limit)` in
  `MemUnreachable.cpp`), while its summary counts all of them. **[sourced]** Real scan durations and
  report sizes: needs a device.
- **Parse without a new dependency.** Yes. The format is line-oriented; the strings above anchor
  it, but simee should parse from a recorded fixture, not from a description.
- **Matches iOS tool:** `leaks`. Differences: no type names (native memory has none), no
  zone, and stacks only if malloc_debug backtraces were enabled before the app started.

### `malloc_debug` backtraces

- **What it gives.** `backtrace[=MAX_FRAMES]` (default 16, max 256) records the stack of every
  allocation. Signal `SIGRTMAX-17` (47) writes "Android Native Heap Dump" to
  `/data/local/tmp/backtrace_heap.<pid>.txt` (Android P+): one line per record with a zygote-child
  flag `z`, `sz`, `num` and the frame addresses `bt`, then a `MAPS` section (`/proc/<pid>/maps`) to
  decode them. `record_allocs` logs every allocation and free as plain text, one line per call (for example
  `186: malloc 0xb6038060 20`: thread id, call, pointer, size), dumped on `SIGRTMAX-18` and not
  carrying stacks;
  `leak_track` dumps live allocations at exit; `check_unreachable_on_signal` (API 34, signal 48)
  runs libmemunreachable on a signal. **[sourced]**
  ([README](https://android.googlesource.com/platform/bionic/+/main/libc/malloc_debug/README.md))
- **Needs.** On a rooted device, `setprop wrap.<pkg> '"LIBC_DEBUG_MALLOC_OPTIONS=backtrace logwrapper"'`
  then `am force-stop <pkg>` (API 26+); verify with `malloc debug enabled` in logcat. Android 12
  has a bug that can break `wrap.<pkg>`; the documented fix is `setprop
  dalvik.vm.force-java-zygote-fork-loop true` followed by `stop` and `start` (a framework restart:
  every app on the device restarts, but malloc_debug stays off for the others). Do not reach for
  `libc.debug.malloc.options` as a fallback: set without `libc.debug.malloc.program` it turns
  malloc_debug on for every process after the restart (the README's "all processes" recipe), which
  slows allocation in every process, not one app. Off a rooted device the page
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

| Mechanism | `ro.debuggable=0` (a user build; `google_apis_playstore` is assumed to be one) | `ro.debuggable=1` (`google_apis`: `su 0`) | `default` AOSP (userdebug-like) |
| --- | --- | --- | --- |
| `dumpsys meminfo <pkg>` and `-d` | Yes **[inferred]** | Yes | Yes |
| `/proc/<pid>/smaps` | Not for another uid; `run-as` for a debuggable app (device check) | Yes | Yes |
| `am dumpheap` managed | Debuggable app only (profileable is refused) | Yes, any app | Yes |
| `dumpsys meminfo --unreachable` | Debuggable or shell-profileable app only; others fail with `Failed to get unreachable memory` (**inferred**) | Same: the app's flags decide, not the build (**inferred**) | Same (**inferred**) |
| heapprofd | Debuggable or profileable app, API 29+ | Nearly any process | Nearly any process |
| `malloc_debug` `wrap.<pkg>` | No (needs `setprop wrap.*`, root, or `wrap.sh` in the APK) | Yes | Yes |

The `ro.debuggable` split is from AMS and `ActivityThread` (`Build.IS_DEBUGGABLE`); which image
tag has which `ro.debuggable` rests on `procs.rs`'s comment that google_apis images have `su`, and
is a device check below. A tool that isn't allowed on an image should
say which image or app flag it needs, as `kill` does for root today.

## 3. Proposed API

The iOS parameters are `device, bundle_id | pid, raw?, timeout_s?(120)`; `mem_vmmap`, `mem_heap`
and `mem_history` keep them, and `mem_leaks` drops `timeout_s` (below). On Android
`bundle_id` is the package name; `pid` must be one of the device's processes (as `processes`
reports); `raw: true` adds the source report as printed; every answer has `pid` and `warnings`,
and the platform column in `help` moves from `iOS` to `Android, iOS` for the methods built.
Each backend runs over adb with the existing deadline and quoting helpers in `sim-android`; no new
dependencies.

### `mem_vmmap` (build)

Source: `su 0 cat /proc/<pid>/smaps` (fallback `run-as` for a debuggable app, to be confirmed);
when neither works, fail with `ro.debuggable`'s value and what to boot instead.
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

Source: `dumpsys -t 40 meminfo --unreachable <pkg>` (the client timeout just above AMS's fixed 30 s
wait, so simee reports AMS's `IoException` rather than its own timeout). **No `timeout_s`
parameter on Android**: it can't extend the scan, and a value above 30 would promise what AMS
refuses. The method says in `help` that the scan stops after 30 s, and fails with that cause when
the report has `Got IoException!` or no summary line. It also fails, with `Failed to get
unreachable memory`, for an app that is neither debuggable nor profileable from shell (**inferred**
from the zygote source, above; confirm on a device): the method says so and names the manifest
flags that would allow it, rather than reporting zero leaks. No root is needed; `raw` is available. Answer matching `{...@mem, leak_count,
stacks, leaks: [object], leaked_bytes?, note?}` plus an Android-only `truncated`:

- `leak_count`, `leaked_bytes` from the summary line (all leaks, not just the listed ones).
- `truncated`: true when `leak_count` exceeds the number of leaks listed. The report lists at most
  100 (`Debug.getUnreachableMemory(100, …)`), largest first, so `leaks` may hold fewer than
  `leak_count` and `sum(leaks[].count)` is under it. Include the listed total beside it
  (`listed_count`) so a caller sees how many are missing; add a `note` saying the cap is AOSP's,
  not simee's. iOS has no cap, so `truncated` is absent there.
- `leaks`: grouped like iOS (`count`, `bytes`, `instances`, `stack`); each instance
  `{address, size}` (no `zone`); the group key is the allocation stack when present and the "similar"
  grouping libmemunreachable prints otherwise; no `type`, since native blocks have none.
- `stacks`: true only when backtraces are in the report. Frames are `@frame`, which requires
  `symbol` (`{address: string, symbol: string, image?: string, source?: string}` in `outputs.rs`):
  `{address, symbol, image?, source?}`, innermost first. Without a symbolizer the frame has no
  name, so `symbol` is the empty string `""` (not omitted, not a made-up name) and `image` is the
  mapping from the dump's `MAPS` section; the address plus `image` is enough to symbolize later.
  Same `note` as iOS when `count > 0 && !stacks`, naming the launch option.
- Launch parameter: `launch {malloc_stack_logging: true}` on Android sets
  `wrap.<pkg>` with `backtrace` before `am start -S` (the per-app route; never
  `libc.debug.malloc.options`, whose scope is every process), and `launch`'s docs row says so.
  Where `wrap.<pkg>` is broken (Android 12+), the launch fails saying to set
  `dalvik.vm.force-java-zygote-fork-loop` and restart the framework, rather than doing it. It needs
  a debuggable build (`su`); without one the launch fails with the reason, not silently without
  stacks.
- `nodes` and `malloced_bytes` (iOS totals): omitted. The log line libmemunreachable writes
  (`N bytes in M allocations unreachable out of X bytes in Y allocations`) goes to logcat, so a
  later pass could read it for these.

### `mem_history` (stays iOS-only for now)

`mem_history {address}` asks "who allocated this address, as events". `malloc_debug`'s backtrace
dump has no per-address query: it's an aggregate by stack (`sz`, `num`, `bt`), with no address and
no frees. `record_allocs` has addresses and frees but no stacks: it writes plain text lines
(`<tid>: malloc <ptr> <size>`, `free <ptr>`, …) for the benchmark replayer, at up to 50 million
records. **[sourced]** (malloc_debug README) So no single mode answers "the allocation stack of
this address": backtraces have stacks and no addresses, `record_allocs` has addresses and no
stacks, and turning on both and joining them is not something the README offers. A reliable
`mem_history` needs a different method, "allocations by stack", not an address lookup. Say so in the API error:
`mem_history is not available on android devices: malloc_debug records allocation sites, not
addresses; use mem_leaks with stacks`, and keep it iOS-only. Revisit only if a device shows an address-to-stack join is possible (for example `backtrace` plus
`record_allocs` in one process, with a pointer in each). This is the only call of the four where
the research answer is "no equivalent that is reliable", which is what #415 asked us to say plainly.

### Not in `mem_*`

- Perfetto/heapprofd recording (profiles, flamegraphs): #167's trace method, with heapprofd as one
  of its data sources. Analysis stays in `ui.perfetto.dev` (protobuf, above).
- Retained size and leak paths (LeakCanary): out of scope.

## 4. Order of work

Each item is one PR, with parsers tested against **recorded** device output as fixtures (fixtures are captured, never
written from a description, as `crates/sim-api/tests/fixtures/memtools/` was for iOS) and the method tested against a fake adb
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
- `getprop ro.debuggable` and whether `su 0` is present on each image type simee provisions, and the
  error `su` gives without it.
- Whether `run-as <pkg> cat /proc/<pid>/smaps` is allowed for a debuggable app under SELinux.
- Whether `showmap` is installed on any image (not needed, but cheap to know).
- `dumpsys meminfo --unreachable` on a release app, on a `profileable` (shell) app and on a
  debuggable app, on a user and on a userdebug image: confirm the inferred failure
  (`Failed to get unreachable memory`) for the release app. Also its time on a small and a large
  heap against AMS's 30 s cap, its effect on the app (the target pauses), and its output with and
  without `wrap.<pkg>` backtraces.
- Whether `wrap.<pkg>` works on API 34–36 images (the README flags an Android 12 bug), and the
  slowdown of `backtrace` on the emulator's app.
- `am dumpheap` on a `profileable` and on a release app on a user build (the source says both are
  refused; confirm the message), and the time and size of the dump for a test app, and how long the app is frozen.
- Time and output size for each call, so `docs/api.md`'s Memory tools section can carry Android
  costs the way it carries iOS's, and `docs/benchmarks.md` a row (AGENTS.md: no published number
  without a measured row).
- Whether an address-to-stack join is possible for `mem_history`.

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
