# Java mod compatibility: status

Status: **ported onto the main tree** (branch `port/native-java-on-main`). Measured 6 November 2026 with fork
binaries built from this branch at 12:24, before its latest commits. The design, tier model and phase plan are
[java-mod-compat.md](java-mod-compat.md); this page is the maintained progress view: what runs today, what the
corpus and the clock measured, and what is left.

A jar runs only when it is explicitly named with `--java-mod PATH` (repeatable) on `horizon-client`. Horizon never
scans a folder for Java jars, and a run without the flag starts no Java runtime. A window runs its jars in the client
role, its server thread in the integrated-server role, and `--dedicated` runs them before the content root is built.
A jar the runtime refuses is logged and skipped, and `HORIZON_JAVA_STRICT=1` makes the dedicated server stop instead
of skipping it. A jar that panics in its entrypoint is isolated; the remaining jars still run.

## What is ported

### `horizon-native-java` (the runtime)

- **Metadata.** Fabric `fabric.mod.json`, NeoForge `neoforge.mods.toml` and legacy Forge `mods.toml`, with identity,
  versions, dependencies, client-only markers and declared entrypoint kinds. Nested JiJ/JarJar jars are reported
  (the corpus probe extracts and probes them) but the game does not descend into them.
- **Entrypoints.** Fabric `main`, `client` and `server` (string, object and array forms) and NeoForge `@Mod` classes.
  The loader runs the first kind it finds for its role (`client`/`main`/`neoforge` on a client,
  `server`/`main`/`neoforge` on a server); a Fabric entrypoint adapter it does not model is refused, which is how
  Kotlin entrypoints fail. `preLaunch` entrypoints are not run.
- **Execution.** A bounded classfile interpreter with exact `long`/`float`/`double` arithmetic, exceptions, records,
  bounded reflection, collections and strings, behind an explicit `Host` boundary. A call the host does not model
  returns a neutral value by default and is counted; the probe fixtures can make it a hard error
  (`with_strict_unknown_host_calls`). The whole run is isolated per jar (`catch_unwind`).
- **Declarations.** Typed `Registry.register`/`DeferredRegister` rows become blocks, items, mob effects, entities,
  menus, block entities, recipes and ingredients, with item/block property inference from ordinary Java builders.
  `META-INF/horizon-native.json` remains the explicit override.
- **Mixin.** Annotation-based plan discovery (config lists, side, priority, selectors) and a classfile writer/weaver
  that rewrites classes whose target is in the same jar: `@Inject` `HEAD`/`RETURN`/`TAIL`, cancellable shapes,
  `CallbackInfoReturnable`, instance callbacks through a generated bridge, condy, stack-map frames. Everything else
  is skipped with a fixed label (`target-outside-jar`, `non-head-anchor`, `unsupported-family`, `cancellable`,
  `selector-modifier`, `not-in-config`, `other-side`, `config-plugin`, `constructor-target`, `unresolved`,
  `target-missing`, `ambiguous-target`, `listed-without-plan`, `weaver-rejected`). The game uses the load-time weave
  only. A jar is labelled `native` only when nothing was skipped, otherwise `native-qualified`.

  The fixture family matrix is **4 applied / 7 missing**: `HEAD`, cancellable `HEAD`, `RETURN` and `TAIL` apply;
  `INVOKE` is skipped as `non-head-anchor`; `FIELD`, `NEW`, `JUMP`, `@Overwrite`, `@ModifyArg` and `@ModifyVariable`
  are skipped as `unsupported-family`. These are the exact load-time labels, not a claim that game-class mixins work.
- **Payloads.** Declared payload channels, codecs (`minecraft:raw`/`byte_array`, `minecraft:string`,
  `minecraft:empty`, opaque fallback), sender/receiver hosts and dispatch of a received frame to the jar's handler.
- **Probe.** `horizon-native-java [--json] [--mixin-plans] [--weave <out.jar>] <jar> [kind] [class]` reports
  entrypoints, host calls, registrations, plans and skips, and can write a woven jar.

### `horizon-client` (the game)

- Java rows are converted into the current content model and layered into the content root: the window's client role
  before the root is built, the integrated server's rows published from the server thread and merged on the next
  rebuild, and the dedicated role before resources. A sorted typed registry snapshot is published beside the root
  and coalesces equal rows; two rows that disagree are refused, not first-wins.
- Declared Java content travels to LAN guests: a bounded, versioned `horizon:java_content` manifest (v3 header with
  epoch and content hash, eight registry id sections, block-entity/menu/entity template rows) is sent at login and
  again when the declaration set changes. A mismatched v3 hash is refused rather than replacing local content.
- Java payload frames travel both ways over LAN on `horizon:payload`, registration on `horizon:payload_channels`;
  frames above the custom-payload envelope are chunked, direction and codec are validated, and `horizon-ctl`'s
  `payload send` injects a declared frame into a hosted world. Each role (client, integrated server, dedicated)
  has its own handler host and queues.
- An opt-in NeoForge loader preflight: when `HORIZON_NEOFORGE_SERVER` (an installed server) and `HORIZON_JVM_BIN` are
  set, the integrated server runs `horizon-jvm --real-loader --entry-kind neoforge` once before its native batch in a
  bounded child process and reports what FancyModLoader saw (completion, mod count, sorted ids, first error).
  A missing binary or server is a report line, not a failure; only `HORIZON_NEOFORGE_STRICT=1` stops startup.
- `server-status` carries bounded `java` and `fml` sections: per-jar loader, engine, backend, weave label, executed
  entrypoints, registrations, opaque rows and isolation failures, `payload_channels`, and the preflight's
  status/role/jars/mods/ids/error. The dedicated log prints one line per started jar. `java-runtime.json` (schema v1,
  256 jars, 256 KiB cap) is written under `HORIZON_HOME` for launcher and support tooling.

### `horizon-jvm` (the reference runner, not the native path)

A standalone JVM runner for the same jars: real Fabric Loader discovery/freeze/entrypoints, an opt-in Knot class
loader (classpath unlock, transformer chain, `--launch-game` against a real 26.3 server), a real
FancyModLoader/NeoForge lifecycle (`--real-loader`, `--list-fml-mods`, `--fml-report`), datagen entrypoints, bounded
mixin-config registration and `--measure`. It is development/reference tooling: no game path runs a world or a jar's
gameplay through it (the only in-game use is the opt-in FML preflight child above), so `backends.jvm` in
`server-status` is always zero and a refused jar is skipped rather than handed to it.

### Tools and tests

`tools/real_mod_corpus.py` (outer + nested probe with a verdict per jar), `tools/java_mod_server_ledger.py` (one real
dedicated server per jar, verdict from its log as `native-ok`/`needs-loader`/`native-gap`; `--docs` regenerates the
table below), `tools/java_mod_loader_ledger.py` (real Fabric loader),
`tools/real_mod_fixtures/build.py` (source-only Fabric/NeoForge/legacy-Forge fixtures), the
`real-mod-corpus` workflow (fixtures and harness self-test only, never third-party jars), the fixture suites under
`crates/horizon-native-java/tests/` and `crates/horizon-jvm/tests/` (the latter skipped unless a Minecraft jar,
loader and libraries are supplied), `tools/native_java_smoke.py`, and the efficiency harness (`tools/efficiency/`,
[efficiency.md](efficiency.md)) behind the numbers below.

## Measured corpus

Two Fabric 26.3 corpora, each jar run once through the real dedicated server
(`horizon-client --java-mod <jar> --dedicated`, fresh scratch home, flat world, `online-mode=false`, 60 s bound, one
process group killed after `Done (`). The [nightly Java corpus workflow](../.github/workflows/nightly-java-corpus.yml)
runs this same probe and ledger over jars already held by its runner. Raw reports:
`~/horizon-corpus/{ledger-wide,ledger-mega}(-classified).{txt,json}`, analysed in `ledger-wide-report.md` and
`mega-report.md`.

| Corpus | Jars | Ran | Refused (native gap) | Skipped | Wall to `Done` per jar |
|---|---:|---:|---:|---:|---|
| wide (`corpus-xl`: 6 packs, SHA-256 deduped) | 223 | 87 (39%) | 35 (16%) | 101 (45%) | mean 3.10 s, max 5.60 s |
| mega (500 unique jars from 50 Fabric 26.3 packs) | 500 | 168 (34%) | 99 (20%) | 233 (47%) | mean 2.67 s, max 5.80 s |

Skipped jars are out of scope for a server, not failures: 69 are client-only and 32 are libraries in the wide
corpus, 179 and 54 in mega. Of the jars that ran, 11 (wide) and 28 (mega) ran with engine `native` (no skipped plan);
the rest ran `native-qualified`. **No mixin was woven in either corpus**: 2,178 plans were skipped in the wide corpus
and 5,689 in mega. Every `ran` row therefore has `woven=0`: the load-time weaver only rewrites targets found in the
same jar, while real mods' plans target `net/minecraft` classes and Horizon has no Java class for those targets. The
dominant skip reason is `target-outside-jar`.

Refusals split by cause (the ledger's own categories): 16 wide / 58 mega fail decoding class metadata (bounded
collection markers, `Integer.parseInt`, `String.split` with a null receiver, malformed `fabric.mod.json`), and
19 / 41 fail in the entrypoint or its bytecode (an uncaught static initialiser, a verifier/opcode gap such as
FarmersDelight `UnsupportedOpcode` or spell_engine `StackUnderflow`, a missing member as in overlaylib, a Kotlin
entrypoint adapter, or a duplicate native declaration; ten of the mega causes were clipped before they printed). The
ledger marks 35 / 99 rows as `native-gap`, 4 / 12 as `needs-loader`, and the remainder as `native-ok` (client-only,
libraries, or already running).

The role-agnostic probe over the same 500 jars (`real_mod_corpus.py`) reports **167 ok (33.4%), 303 fallback (60.6%),
30 unsupported (6.0%)**, with the top non-ok groups being "no main entrypoint but needs the loader/embedded JVM",
client-only entrypoints, and malformed metadata.

## What runs native, what needs the loader, why

Every ledger row ends in one of three labels, so a verdict says what to do instead of counting a refusal:

- `native-ok`: the dedicated server runs the jar on its own. Either it executes its entrypoint and no skipped plan
  aims at the game, or it is a client-only jar or a library the server role never loads.
- `needs-loader`: it runs, but at least half of its skipped mixin plans target game classes (`target-outside-jar`), so
  only a real loader applies them.
- `native-gap`: native execution refuses it yet; [Known gaps](#known-gaps) and the row's reason say where.

The block below is generated from the wide corpus's committed ledger, and the tool rewrites it in place
(`--docs`), so it cannot drift from the ledger format:

<!-- java-mod-ledger:start -->
_Generated by `tools/java_mod_server_ledger.py --docs docs/java-mod-compat-status.md` from `tools/real_mod_fixtures/LAST_SERVER_LEDGER_223.txt`; edit the tool, not this block._

**223 jars: 173 `native-ok`, 4 `needs-loader`, 46 `native-gap`.**

| Label | Jars | What it means | Why |
|---|---:|---|---|
| `native-ok` | 173 | the dedicated server runs it on its own | 71 ran, every skipped plan stayed inside the jar; 46 client-only (the server role has no entrypoint to run); 44 libraries (nothing for the server to execute); 12 ran with no plan skipped |
| `needs-loader` | 4 | it runs, but its plans target the game, so only a real loader applies them | 4 ran, but their skipped plans target the game |
| `native-gap` | 46 | native execution cannot carry it yet | 19 refused for another cause; 16 refused while decoding metadata; 11 refused before a server entrypoint matched |

**Jars that are not `native-ok` (50).**

| Jar | Label | Why |
|---|---|---|
| `456640-8026052.jar` | `needs-loader` | ran; 1 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `armor-stand-poses-fabric-26.3-1.4.0.jar` | `needs-loader` | ran; 6 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `baguettelib-26.3-Fabric-2.0.7.jar` | `needs-loader` | ran; 19 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `naturally_trimmed-3.6.1+26.3-fabric.jar` | `needs-loader` | ran; 5 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `1005914-8891048.jar` | `native-gap` | refused: `Fabric entrypoint adapter kotlin is not supported by the ...` |
| `257814-8894326.jar` | `native-gap` | refused: `uncaught exception java/lang/AssertionError (at...` |
| `326652-8890593.jar` | `native-gap` | refused while decoding metadata: `Metadata("String#split has a null receiver (at com/cupboard/confi...` |
| `531761-8889669.jar` | `native-gap` | refused: `uncaught exception java/lang/NullPointe...` |
| `535489-8909383.jar` | `native-gap` | refused: `uncaught exception java/lang/IllegalS...` |
| `608235-8905060.jar` | `native-gap` | refused while decoding metadata: `Metadata("java/lang/Integer#parseInt expec...` |
| `623764-8914234.jar` | `native-gap` | refused: `uncaught exception java/lang/Illeg...` |
| `BetterAdvancements-Fabric-26.3-0.6.0.78.jar` | `native-gap` | refused before a server entrypoint matched: `no native Fabric or NeoForge entrypoint for server execution (availabl...` |
| `DetailArmorBarReconstructed-5.3.2-26.3-fabric.jar` | `native-gap` | refused before a server entrypoint matched: `no native Fabric or NeoForge entrypoint for server execution (av...` |
| `EuphoriaPatcher-1.10.5-r5.9.3-fabric.jar` | `native-gap` | refused (the ledger's line was clipped before the cause) |
| `InventoryProfilesNext-fabric-26.3-2.3.8.jar` | `native-gap` | refused before a server entrypoint matched: `no native Fabric or NeoForge entrypoint for server execution (availabl...` |
| `Legacy4J-26.3-1.9.1-fabric.jar` | `native-gap` | refused while decoding metadata: `Metadata("malformed java/lang/Boolean marker (at wily/...` |
| `XaeroPlus-2.37.0+fabric-26.3-WM1.47.0-MM26.6.0.jar` | `native-gap` | refused before a server entrypoint matched: `no native Fabric or NeoForge entrypoint for server execution (a...` |
| `betterstats-5.6.0-beta.2+fn-26.3.jar` | `native-gap` | refused: `uncaught exceptio...` |
| `betterstats-5.7.0+fn-26.3.jar` | `native-gap` | refused: `uncaught exception java/...` |
| `carpet-tis-addition-v1.82.4-mc26.3.jar` | `native-gap` | refused while decoding metadata: `Metadata("malformed java/lan...` |
| `clientsort-fabric-3.104.1+26.3.jar` | `native-gap` | refused while decoding metadata: `Metadata("expected a native ...` |
| `collective-26.3.0-8.46.jar` | `native-gap` | refused while decoding metadata: `Metadata("expected a native collection ...` |
| `dungeons-and-taverns-6.0.2.jar` | `native-gap` | refused while decoding metadata: `Metadata("resource fabric.mod.json exceeds bounded native size limits")` |
| `entity_model_features-3.3.12-26.3-fabric.jar` | `native-gap` | refused before a server entrypoint matched: `no native Fabric or NeoForge entrypoint for server execution (availab...` |
| `entity_texture_features-7.2.6-26.3-fabric.jar` | `native-gap` | refused before a server entrypoint matched: `no native Fabric or NeoForge entrypoint for server execution (availa...` |
| `glowing-torchflower-fabric-mc26.2-1.4.1.jar` | `native-gap` | refused before a server entrypoint matched: `no native Fabric or NeoForge entrypoint for server execution (availabl...` |
| `gugle-carpet-addition-v2.12.9+build.104.jar` | `native-gap` | refused before a server entrypoint matched: `no native Fabric or NeoForge entrypoint for server execution (availabl...` |
| `kreno_fpatcher-26.2.0+26.1.jar` | `native-gap` | refused: `uncaught exception java/lang/SecurityExcepti...` |
| `main-menu-credits-1.4.0+26.2-universal.jar` | `native-gap` | refused (the ledger's line was clipped before the cause) |
| `moderner-beta-fabric-5.0.0-alpha.4+26.3.jar` | `native-gap` | refused while decoding metadata (the ledger's line was clipped before the cause) |
| `overlaylib-0.12.1+26.3.jar` | `native-gap` | refused: `MissingMethod("#getInstance")` |
| `peek-fabric-1.6.0+26.3.jar` | `native-gap` | refused while decoding metadata: `Metadata("expected a native collection marker, got Some(...` |
| `pickupnotifications-fabric-3.0.1+26.3.jar` | `native-gap` | refused (the ledger's line was clipped before the cause) |
| `puzzleslib-v26.3.12-mc26.3.x+fabric.jar` | `native-gap` | refused: `uncaught exception ja...` |
| `puzzleslib-v26.3.13-mc26.3.x+fabric.jar` | `native-gap` | refused: `uncaught exception ja...` |
| `puzzleslib-v26.3.14-mc26.3.x+fabric.jar` | `native-gap` | refused: `uncaught exception ja...` |
| `reeses-sodium-options-fabric-2.2.5+mc26.3.jar` | `native-gap` | refused before a server entrypoint matched: `no native Fabric or NeoForge entrypoint for server execution (availa...` |
| `reeses-sodium-options-fabric-2.2.6-beta.1+mc26.3.jar` | `native-gap` | refused before a server entrypoint matched: `no native Fabric or NeoForge entrypoint for server execution ...` |
| `respackopts-26.3.0.jar` | `native-gap` | refused while decoding metadata: `Metadata("expected a nati...` |
| `rrls-5.2.8+mc.26.3.jar` | `native-gap` | refused while decoding metadata: `Metadata("expected a nati...` |
| `scribble-2.1.0-beta1+mc26.3.jar` | `native-gap` | refused: `uncaught exception java/lan...` |
| `servercore-fabric-1.5.20+26.3.jar` | `native-gap` | refused while decoding metadata: `Metadata("expect...` |
| `simple_music_control-fabric-1.5.5+26.3.jar` | `native-gap` | refused before a server entrypoint matched: `no native Fabric or NeoForge entrypoint for server execution (available...` |
| `swinginglanterns-fabric-26.3-2.0.3.jar` | `native-gap` | refused while decoding metadata: `Metadata(...` |
| `tcdcommons-5.6.0-beta.2+fn-26.3.jar` | `native-gap` | refused: `uncaught exception jav...` |
| `tcdcommons-5.7.0+fn-26.3.jar` | `native-gap` | refused: `uncaught exception java/lang/...` |
| `vmp-fabric-mc26.3-0.2.0+beta.7.238-all.jar` | `native-gap` | refused while decoding metadata: `Metadata("java/lang/Integer#parseInt exp...` |
| `whiteout-0.21.0.jar` | `native-gap` | refused: `uncaught exception java/lang/IllegalStateExceptio...` |
| `xaeroworldmap-fabric-26.3-1.47.0.jar` | `native-gap` | refused while decoding metadata: `Metadata("malformed java/lang/Boolean mark...` |
| `zoomify-2.17.0+mc26.3-universal.jar` | `native-gap` | refused while decoding metadata: `Metadata("malformed...` |
<!-- java-mod-ledger:end -->

After a ledger run, regenerate the block without touching a jar set again:

```sh
python3 tools/java_mod_server_ledger.py --from tools/real_mod_fixtures/LAST_SERVER_LEDGER_223.txt \
  --docs docs/java-mod-compat-status.md
```

`--table` prints the same block without writing it, and `--table-limit N` sets how many per-jar rows it keeps.

## Measured efficiency

Host: 16 CPUs, load average 9.6–13 during the runs (shared box). Medians; client figures are medians of three
interleaved runs in rotated order, server figures median of six. The rerun recipe is [efficiency.md](efficiency.md)
(`tools/efficiency/`); this run's raw reports are in `~/horizon-corpus/` (`EFFICIENCY.md`, `efficiency-*.json`, and
the `efficiency-scripts/` the suite was written from).

### Dedicated server, vanilla 26.3 vs Horizon (no java mods), same `server.properties`

| Metric (median) | Vanilla 26.3 | Horizon | Difference |
|---|---:|---:|---:|
| Wall to listening port | 7.09 s | 3.22 s | **2.2× faster** |
| Wall to `Done (` | 7.86 s | 3.22 s | **2.4× faster** |
| Server's own `Done (x.xxxs)` timer | 0.61 s | 3.20 s | 5.2× slower, different span* |
| RSS at ready | 1312 MiB | 248 MiB | **5.3× less** |
| Threads at ready | 102 | 21 | **4.9× fewer** |

\* Vanilla's timer starts after ~7.3 s of JVM startup, registries, datafixers and its 68 MiB library bundle; Horizon's
3.20 s is effectively the whole process (resources 1.9 s, worldgen data 0.39 s, level prep). Wall clock is the honest
comparison, where Horizon wins by 4.6 s. Horizon opens the listening socket at `Done` (0.04 ms gap), so a client
never connects to a world that is not ready.

### Client scaling with native java mods (offscreen, render distance 8, `maxFps` 260)

0/3/20/74 jars are filename-sorted slices of the natively-runnable set, not a curated pack.

| Jars | Effective client mods | Startup | RSS | avg fps | p50 | p95 | low 1% |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 0 | 0 | 4080 ms | 1179 MiB | 1556 | 0.62 ms | 0.89 ms | 494 |
| 3 | 3 | 4150 ms | 1229 MiB | 1714 | 0.58 ms | 0.64 ms | 543 |
| 20 | 19 | 5150 ms | 1698 MiB | 1529 | 0.62 ms | 0.82 ms | 452 |
| 74 | 65 | 8410 ms | 2089 MiB | 1629 | 0.60 ms | 0.67 ms | 564 |

Frame time is mod-insensitive in this fixture: tick and update stay ~0.01 ms and render ~0.2 ms per frame; the mod
tax is startup (the Java entrypoint plus its share of resource work, ~60 ms per effective mod) and memory
(~14 MiB per mod). Threads stay at 61 regardless of jar count.

### Horizon dedicated startup with native jars

| Jars passed | Effective server mods | Time to listening port/`Done` | RSS | Threads |
|---:|---:|---:|---:|---:|
| 0 | 0 | 2.39 s | 265 MiB | 21 |
| 25 | 25 | 3.26 s | 544 MiB | 21 |
| 50 | 50 | 3.82 s | 700 MiB | 21 |
| 74 | 51 | 4.39 s | 695 MiB | 21 |
| 89 (all native-ok) | 52 | 5.22 s | 689 MiB | 21 |

Only 52 of the 89 natively-runnable jars declare a dedicated-server entrypoint, so the 74-jar point is 51 executed
mods and time/RSS flatten past 50. There were zero fallbacks to the JVM and zero startup failures in any run.

### 500-jar native profile

The 500-jar profile reaches `Done` in **13.7 s** at **833 MiB peak RSS**. **6.5 s** of that run is parse work for
jars that are later skipped or otherwise do not execute. RSS follows a parse-count model: a fixed process baseline
plus the classfile and metadata retained for every jar parsed, so filtering a jar after parsing does not reclaim its
share of the peak. The profile is therefore a parser/loader measurement, not a claim that 500 effective server mods
ran.

Caveats: the host was loaded (absolute fps is depressed and noisy); vanilla ran `-Xmx2G` and RSS is measured at
`Done`; the client ran offscreen with `maxFps=260` because the default 120 fps cap hides all frame differences.

## Known gaps

1. **Game-class mixins do not apply.** The load weave only rewrites classes inside the jar; a plan whose target is a
   Minecraft class is skipped. That is every plan that matters (0 woven; 2,178 and 5,689 skipped), so `ran` means
   "entrypoint executed, mixins skipped, vanilla server reached `Done`", not "the mod works".
2. **Refusals.** 35 of 122 wide and 99 of 267 mega jars that declare a server entrypoint still fail (29%/37%). The
   causes are named above and in the ledgers. The raw log line is bounded at 160 characters, so a human sees a clipped
   cause; the ledger recovers the skip class from the jar's own metadata.
3. **Nested and non-server entrypoints.** Nested JiJ/JarJar jars are probed but never bootstrapped in-game, `preLaunch`
   is not run, and an unmodelled Fabric entrypoint adapter (Kotlin) is refused.
4. **Callbacks.** Only entrypoints and received-payload handlers execute. Event listeners (`ServerTickEvent`,
   Fabric lifecycle/tick events, client tick/render events) are recorded by the host but the game never dispatches
   them; there is also no compiled HUD, key-binding, tooltip, screen, renderer or model façade in this branch. The
   former façade and census sources were deleted; they are recoverable from commit `be8567b9a`, so client jars
   register content but do not draw.
5. **Content consumption.** Blocks, items, effects, entities and menus reach the content model. Block entities,
   recipes and ingredients are recorded and warned as not expressible; data components and attachments have a
   manifest section but no declaration table; a guest stores the login manifest, but local command and content
   lookups do not consume the stored rows yet. There is no mod hot reload (content rows arriving late rebuild the
   root once).
6. **No JVM fallback in the game.** `horizon-jvm` is a standalone reference runner; a jar the native runtime refuses
   is skipped, so the `jvm`/`sidecar` counts in `server-status` are structurally zero.
7. **Diagnostics are partial.** Refused jars are logged but absent from the `server-status` rows (it answers for the
   jars that started); `java-runtime.json` writes its `woven` and `skipped` fields as `0`/`None`, so the launcher's
   runtime panel labels a jar `needs-loader` from its skip reason but cannot yet list the plans behind it.
8. **NeoForge.** Native metadata and `@Mod` bootstrap work. The real FML lifecycle is a bounded, opt-in child
   process (`horizon-jvm --real-loader`) used as a preflight on the integrated server, not the path a mod's code
   takes; the dedicated role does not preflight yet, there is no in-game loader selection, and no automatic
   intermediary-to-official remapping.
9. **The numbers are narrow.** Both corpora are filename-sorted slices of 26.3 Fabric jars, the client run is
   offscreen with no mod drawing, and the efficiency suite has no steady-state or playable-session figure.

## Planned work

1. **Make mixins count.** Map skipped game-class plans onto native hook points (or provide the target classes through
   the loader path), starting with the plan families the corpora actually declare. Gate: `woven > 0` in a real-mod
   ledger, and every `native` label backed by zero skipped plans.
2. **Close metadata refusals.** Bounded native collection markers, exact `Integer.parseInt`, `String.split` receivers
   and tighter `fabric.mod.json` bounds. Gate: metadata refusals 74 → 0 across both corpora.
3. **Close runtime refusals.** Entrypoint static-init failures, `UnsupportedOpcode`/`StackUnderflow` verifier gaps,
   missing members and the Kotlin adapter. Gate: refused share of in-scope server jars below 5%.
4. **Finish content.** Block entities, recipes and ingredients in the content model; data-component and attachment
   declaration tables; guests consume the applied manifest in command/content lookups; then mod hot reload.
5. **Client surface.** Compile the façade client families (HUD, keys, tooltips, screens, renderers, models) into the
   runtime and drive them from the game; measure with rendering on and a curated pack, not offscreen.
6. **NeoForge path.** Finish the FML path: give the dedicated role the integrated server's opt-in preflight, then
   in-process loader selection, with remap readiness checked against a real NeoForge 26.3 server.
7. **Diagnostics.** Populate the runtime report's weave counts and first skip reason, include refused jars in
   `server-status`, stop clipping the refusal cause, and surface a per-mod compatibility row (with cache clear) in the
   launcher.
8. **Keep the corpus honest.** Refresh the 223/500 ledgers per release, fail a run when `ran` regresses or a refused
   row loses its cause, and extend the fixture workflow (it deliberately never downloads third-party jars).
9. **Performance.** Rerun the efficiency suite with a curated pack and steady-state heap, and a playable client
   session with mods that actually draw.

## How to reproduce

```sh
cargo build --profile fast -p horizon-client -p horizon-native-java -p horizon-jvm
cargo test -p horizon-native-java
python3 tools/real_mod_corpus.py --self-test
python3 -m unittest tests.test_real_mod_corpus tests.test_real_mod_fixtures

# a corpus through the real dedicated server (one fresh server per jar; 60 s bound)
scratch=$(mktemp -d)
TMPDIR=$scratch HORIZON_NO_AUDIO=1 python3 tools/java_mod_server_ledger.py ~/horizon-corpus/corpus-xl \
  --binary target/fast/horizon-client --resources .local/horizon-client/26.3 \
  --limit 300 --timeout 60 --json --output ledger.txt > ledger.json

# the same jars through the real Fabric loader (needs the game jar, loader and libraries)
python3 tools/java_mod_loader_ledger.py --mods-dir ~/horizon-corpus/corpus-xl \
  --binary target/fast/horizon-jvm --json
```

In the game: `horizon-client --java-mod mods/example.jar --resources .local/horizon-client/26.3 --world W`, add
`--java-mod` once per jar, and put `--java-mod` before `--dedicated` on a server. `HORIZON_JAVA_STRICT=1` turns a
refusal into a startup failure. The efficiency harness is in `tools/efficiency/` ([efficiency.md](efficiency.md)):
`python3 tools/efficiency/run_server.py --runs 3`,
`python3 tools/efficiency/run_client.py --counts 0,3,20,74 --max-fps 260 --warmup 5` and
`python3 tools/efficiency/run_startup.py --counts 0,25,50,74`; the run's own reports and its jar-selection probe stay
in `~/horizon-corpus/`.
