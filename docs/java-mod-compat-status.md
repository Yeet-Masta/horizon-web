# Java mod compatibility: status

Status: **ported onto the main tree** (branch `port/native-java-on-main`). Measured 6 November 2026 with
binaries built from this branch at 19:02. The design, tier model and phase plan are
[java-mod-compat.md](java-mod-compat.md); this page is the maintained progress view: what runs today, what the
corpus and the clock measured, and what is left.

A jar runs only when it is explicitly named with `--java-mod PATH` (repeatable) on `horizon-client`. Horizon never
scans a folder for Java jars, and a run without the flag starts no Java runtime. A window runs its jars in the client
role, its server thread in the integrated-server role, and `--dedicated` runs them before the content root is built.
A jar the runtime refuses is logged and skipped, and `HORIZON_JAVA_STRICT=1` makes the dedicated server stop instead
of skipping it. A jar that panics in its entrypoint is isolated; the remaining jars still run.

## The goal: two phases to native-only

The project has one destination and one checkpoint on the way. Both phases are gated on the measured corpus, not on a
date:

| Phase | Goal | Exit criteria |
|---|---|---|
| **1. Every corpus jar works on a tier** | no measured jar is left without a path: it runs natively, or through the loader tier, and its label says why | **0 `native-gap`** rows in both committed ledgers; **every `needs-loader` jar proven on the loader tier** — a real Fabric-loader run reaches `Done (`, lists the jar's mod id and names no exception against it; and a hybrid pack session completes both halves inside its work bound |
| **2. Native-only** | the same corpus runs on `horizon-native-java` alone; the loader tier is retired and no game path starts a JVM | 0 `needs-loader` and 0 `native-gap`; every exercised mixin plan is applied by the native weaver or bound to a named host hook; `backends.jvm` stays zero because nothing has a JVM to fall back to |

Phase 1 is a checkpoint, not the product: a `needs-loader` jar is proven, not ported. Skipped client-only jars and
libraries stay `native-ok` throughout — the server role has nothing of theirs to run, so they are not failures.
Phase 2 starts from phase 1's zero-refusal baseline and adds what the loader was carrying: game-class mixins, event
callbacks and the client surface. The design and tier model are
[java-mod-compat.md](java-mod-compat.md#native-first-strategy-and-tiers).

### Coverage of the two gates (6 November 2026)

| Corpus | Jars | `native-ok` | `needs-loader` | of those proven on the loader | `native-gap` | Phase 1 met? |
|---|---:|---:|---:|---:|---:|---|
| wide (`corpus-xl`) | 223 | 151 (68%) | 45 (20%) | 25/45 (56%) | 27 (12%) | no |
| mega (500 unique) | 500 | 346 (69%) | 86 (17%) | 25/86 (29%) | 68 (14%) | no |

"Proven" counts a `needs-loader` jar only when one of the three committed loader ledgers
(`LAST_LOADER_LEDGER.txt`, `LAST_LOADER_LEDGER_XPLUS.txt`, `LAST_LOADER_LEDGER_HELIX.txt`) has a row with
`status=listed`, no `first_exception`, matched by file name or mod id. A row with `status=unlisted` (or `error`) is
seen but not proven: 6 wide / 13 mega. A jar with no loader row at all is unmeasured: 14 wide / 48 mega. The loader
ledgers cover other corpus slices (corpus-xl, corpus-xplus, Helix), not one run over both corpora, so these are
evidence of the loader tier, not full proof of it.

The same runs show the distance to phase 2: 0 mixins were woven in either corpus, with 2,944 (wide) and 7,742 (mega)
plans skipped, overwhelmingly `target-outside-jar`. Of the 121 wide and 265 mega jars that declare a server
entrypoint, 94 and 197 reached `Done`; the refusals are the `native-gap` rows above.

### Blockers

**Phase 1:**

1. **Native refusals (27 wide, 68 mega).** 9 wide / 29 mega fail decoding class metadata; 18 / 34 fail with an
   uncaught exception in the entrypoint; mega adds one verifier refusal, two nested-jar-frame cases, one duplicate
   declaration and one unreadable metadata file. The gate is 0, so every row needs either a fix or a reclassification
   backed by a loader proof.
2. **Loader proof is partial.** 25 of 45 wide and 25 of 86 mega `needs-loader` jars have a `listed` loader row; the
   rest were seen but not listed, or never ran under the loader. Each needs a loader run that reaches `Done (`,
   lists it and records no exception against it.
3. **The hybrid session does not finish.** The 112-jar Helix pack splits into 75 native and 33 loader jars; the
   loader half loads 67 mods and applies 346 mixins with 0 failures, but times out at 180 s while transforming
   `NaturalSpawner`, and the static check reports 5 cross-partition dependency conflicts. A jar proven alone is not
   yet proven in the pack a player runs.
4. **NeoForge is outside the measured corpus.** Both corpora are Fabric 26.3. If a NeoForge corpus joins phase 1,
   the dedicated role still lacks the FML preflight and there is no automatic intermediary-to-official remapping.

**Phase 2:**

1. **Game-class mixins do not apply.** The load-time weaver only rewrites same-jar targets, and the two opt-in hook
   points (`player_tick`, `item_picked_up`) are not fired by game code. Closing this is the whole distance for the
   45 / 86 `needs-loader` jars.
2. **Callbacks and the client surface.** Only entrypoints and received-payload handlers execute; event listeners are
   recorded but never dispatched, and there is no compiled HUD, key-binding, tooltip, screen, renderer or model
   façade, so client jars register content but do not draw.
3. **Content consumption.** Block entities, recipes and ingredients are recorded but not expressible; data components
   and attachments have no declaration table; a guest stores the login manifest but local lookups do not consume it;
   there is no mod hot reload.
4. **No fallback by definition.** Phase 2 keeps phase 1's zero-refusal baseline and moves the 45 / 86
   `needs-loader` jars onto the native engine; the opt-in FML preflight and `horizon-jvm` remain development tools,
   never a quiet fallback, and the NeoForge lifecycle needs native coverage if the corpus widens.

### Measuring the gates

```sh
# the label gates, from the committed ledgers (no jars, no servers)
python3 tools/java_mod_server_ledger.py --from tools/real_mod_fixtures/LAST_SERVER_LEDGER_223.txt | tail -1
python3 tools/java_mod_server_ledger.py --from tools/real_mod_fixtures/LAST_SERVER_LEDGER_500.txt | tail -1

# a fresh corpus through the real dedicated server (one fresh server per jar, 60 s bound)
scratch=$(mktemp -d)
TMPDIR=$scratch HORIZON_NO_AUDIO=1 python3 tools/java_mod_server_ledger.py ~/horizon-corpus/corpus-xl \
  --binary target/fast/horizon-client --native-binary target/fast/horizon-native-java \
  --resources .local/horizon-client/26.3 --timeout 60 --json --output ledger.txt > ledger.json

# loader-tier proof (needs the pinned game jar, loader and libraries)
python3 tools/java_mod_loader_ledger.py --mods-dir ~/horizon-corpus/corpus-xl \
  --binary target/fast/horizon-jvm --output tools/real_mod_fixtures/LAST_LOADER_LEDGER.txt

# the hybrid session, both halves, on the measured pack
python3 tools/hybrid_pack.py ~/horizon-corpus/corpus-helix \
  --ledger tools/real_mod_fixtures/LAST_SERVER_LEDGER_223.txt \
  --ledger tools/real_mod_fixtures/LAST_SERVER_LEDGER_500.txt --match-by-hash --measure
```

## What is ported

### `horizon-native-java` (the runtime)

- **Metadata.** Fabric `fabric.mod.json`, NeoForge `neoforge.mods.toml` and legacy Forge `mods.toml`, with identity,
  versions, dependencies, client-only markers and declared entrypoint kinds. Nested JiJ/JarJar jars are reported
  (the corpus probe extracts and probes them) but the game does not descend into them.
- **Entrypoints.** Fabric `main`, `client` and `server` (string, object and array forms) and NeoForge `@Mod` classes.
  The loader runs the first kind it finds for its role (`client`/`main`/`neoforge` on a client,
  `server`/`main`/`neoforge` on a server). The Kotlin adapter is resolved to the default invocation contract (its
  classes compile to ordinary JVM bytecode), and an adapter it does not model is refused. `preLaunch` entrypoints
  are not run.
- **Execution.** A bounded classfile interpreter with exact `long`/`float`/`double` arithmetic, exceptions, records,
  bounded reflection, collections and strings, behind an explicit `Host` boundary. A call the host does not model
  returns a neutral value by default and is counted; the probe fixtures can make it a hard error
  (`with_strict_unknown_host_calls`). The whole run is isolated per jar (`catch_unwind`).
- **Declarations.** Typed `Registry.register`/`DeferredRegister` rows become blocks, items, mob effects, entities,
  menus, block entities, recipes and ingredients, with item/block property inference from ordinary Java builders.
  `META-INF/horizon-native.json` remains the explicit override.
- **Mixin.** Annotation-based plan discovery (config lists, side, priority, selectors) and a classfile writer/weaver
  that rewrites classes whose target is in the same jar. All eleven fixture families weave: `HEAD`, cancellable
  `HEAD`, `RETURN`, `TAIL`, `INVOKE`, `FIELD`, `NEW`, `JUMP`, `@Overwrite`, `@ModifyArg` and `@ModifyVariable`,
  with `CallbackInfo`/`CallbackInfoReturnable`, instance callbacks through a generated bridge, condy and stack-map
  frames. A plan the weaver cannot carry stays skipped with a fixed label (`target-outside-jar`, `non-head-anchor`,
  `unsupported-family`, `cancellable`, `selector-modifier`, `not-in-config`, `other-side`, `config-plugin`,
  `constructor-target`, `unresolved`, `target-missing`, `ambiguous-target`, `listed-without-plan`, `weaver-rejected`).
  The game uses the load-time weave only: a game-class target still has no class to rewrite, so a real jar reads
  `native-qualified`. The opt-in `plan_load_time_weave_with_game_hooks` tier can bind an `@Inject` for a game class
  to one of two named host hook points (`player_tick`, `item_picked_up`) and run its callback there; the game's
  loader does not call it yet, because no game code fires those registrations.

  The fixture family matrix is **11 applied / 0 missing**: every family above weaves and runs with its own argument
  at its own position (`crates/horizon-native-java/tests/fixture_mixin_family_matrix.rs`). This is the exact
  load-time path the game calls, not a claim that game-class mixins work: those plans still need the loader or the
  opt-in hook tier.
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
dedicated server per jar, verdict from its log, `java-runtime.json` and plan report as
`native-ok`/`needs-loader`/`native-gap`; `--docs` regenerates the
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
`mega-report.md`; the committed ledgers are `tools/real_mod_fixtures/LAST_SERVER_LEDGER_{223,500}.txt`.

| Corpus | Jars | Ran | Refused (native gap) | Skipped | Wall to `Done` per jar |
|---|---|---:|---:|---:|---|
| wide (`corpus-xl`: 6 packs, SHA-256 deduped) | 223 | 94 (42%) | 27 (12%) | 102 (46%) | mean 2.42 s, max 5.66 s |
| mega (500 unique jars from 50 Fabric 26.3 packs) | 500 | 197 (39%) | 68 (14%) | 235 (47%) | mean 2.26 s, max 8.75 s |

Skipped jars are out of scope for a server, not failures: 69 are client-only and 33 are libraries in the wide
corpus, 180 and 55 in mega. Of the jars that ran, 13 (wide) and 37 (mega) ran with engine `native` (no skipped plan);
the rest ran `native-qualified`. **No mixin was woven in either corpus**: 2,944 plans were skipped in the wide corpus
and 7,742 in mega. Every `ran` row therefore has `woven=0`: the load-time weaver only rewrites targets found in the
same jar, while real mods' plans target `net/minecraft` classes and Horizon has no Java class for those targets. The
dominant skip reason is `target-outside-jar`; 45 (wide) and 86 (mega) of the running jars have at least half of
their skipped plans aimed at the game and are labelled `needs-loader`.

Refusals split by cause (the ledger's own categories): 9 wide / 29 mega fail decoding class metadata (bounded
collection markers, `ServiceLoader` and stream markers, `String.split`/`Map#keySet` null receivers, `Gson` needing an
`Appendable`, malformed `fabric.mod.json`), and 18 / 34 fail with an uncaught exception in the entrypoint (a null
config or listener phase, reflection on a null class, a mixed-loader guard, a mixin plugin that did not initialise).
Mega adds one verifier refusal (`StackUnderflow` in spell_engine's datagen), two jars that exceed the interpreter's
128 nested-jar-frame bound, one duplicate native item declaration, and one jar whose metadata cannot be read at all.
The Kotlin entrypoint adapter now runs (`fzzy_config` starts), so no refusal is attributed to it. The ledger marks
27 / 68 rows as `native-gap`, 45 / 86 as `needs-loader`, and the remainder, 151 / 346, as `native-ok` (client-only,
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

**223 jars: 151 `native-ok`, 45 `needs-loader`, 27 `native-gap`.**

| Label | Jars | What it means | Why |
|---|---:|---|---|
| `native-ok` | 151 | the dedicated server runs it on its own | 69 client-only (the server role has no entrypoint to run); 36 ran, every skipped plan stayed inside the jar; 33 libraries (nothing for the server to execute); 13 ran with no plan skipped |
| `needs-loader` | 45 | it runs, but its plans target the game, so only a real loader applies them | 45 ran, but their skipped plans target the game |
| `native-gap` | 27 | native execution cannot carry it yet | 18 refused by an uncaught exception in its entrypoint; 9 refused while decoding metadata |

**Jars that are not `native-ok` (72).**

| Jar | Label | Why |
|---|---|---|
| `1283899-8898481.jar` | `needs-loader` | ran; 25 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `248787-8907912.jar` | `needs-loader` | ran; 11 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `326652-8890593.jar` | `needs-loader` | ran; 9 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `348831-8926733.jar` | `needs-loader` | ran; 18 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `367706-8917645.jar` | `needs-loader` | ran; 277 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `368825-8901843.jar` | `needs-loader` | ran; 9 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `410295-8911219.jar` | `needs-loader` | ran; 8 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `419699-8915075.jar` | `needs-loader` | ran; 124 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `456640-8026052.jar` | `needs-loader` | ran; 1 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `511733-8929530.jar` | `needs-loader` | ran; 10 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `526901-8925000.jar` | `needs-loader` | ran; 1 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `608235-8905060.jar` | `needs-loader` | ran; 15 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `639842-8906536.jar` | `needs-loader` | ran; 38 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `734339-8931641.jar` | `needs-loader` | ran; 4 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `832882-8896904.jar` | `needs-loader` | ran; 31 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `882495-8910991.jar` | `needs-loader` | ran; 2 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `888790-8002356.jar` | `needs-loader` | ran; 5 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `892086-8902974.jar` | `needs-loader` | ran; 8 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `BedrockEntityModels-26.3-1.1.2-fabric.jar` | `needs-loader` | ran; 15 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `BedrockSkins-26.3-2.1.1.jar` | `needs-loader` | ran; 88 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `CustomPlayerModels-Fabric-26.3-0.6.27e.jar` | `needs-loader` | ran; 60 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `InventoryInteractions-2.2.0+26.3+fabric.jar` | `needs-loader` | ran; 6 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `InventoryParticles-3.2.0+26.3+fabric.jar` | `needs-loader` | ran; 20 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `Jade-mc26.3-Fabric-26.3.5.jar` | `needs-loader` | ran; 22 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `PatPat-1.3.2+26.3+fabric.jar` | `needs-loader` | ran; 31 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `advancementscreenshot-26.3.0-5.3.jar` | `needs-loader` | ran; 6 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `architectury-fabric-22.0.4.jar` | `needs-loader` | ran; 124 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `armor-stand-poses-fabric-26.3-1.4.0.jar` | `needs-loader` | ran; 6 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `baguettelib-26.3-Fabric-2.0.7.jar` | `needs-loader` | ran; 19 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `bannerpoint-fabric-1.1.2+26.3.jar` | `needs-loader` | ran; 17 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `cropsloverain-3.2.1.jar` | `needs-loader` | ran; 16 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `custom-glow-enchantment-outline-fabric-26.3-1.2.3.jar` | `needs-loader` | ran; 23 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `deleteworldstotrash-v26.3.0-mc26.3.x+fabric.jar` | `needs-loader` | ran; 4 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `fix-keyboard-on-linux-1.0.2.jar` | `needs-loader` | ran; 2 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `immersive-hotbar-1.2.2-26.3.jar` | `needs-loader` | ran; 20 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `jei-26.3-fabric-31.16.0.83.jar` | `needs-loader` | ran; 24 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `light-overlay-2.13.0+26.3.jar` | `needs-loader` | ran; 3 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `mcwifipnp-2.1.4-26.3-fabric.jar` | `needs-loader` | ran; 4 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `modernregsyncfix-2.0.0.jar` | `needs-loader` | ran; 5 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `naturally_trimmed-3.6.1+26.3-fabric.jar` | `needs-loader` | ran; 5 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `peek-fabric-1.6.0+26.3.jar` | `needs-loader` | ran; 40 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `simplemenu-26.3.0-2.3.jar` | `needs-loader` | ran; 39 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `smooth_f5-fabric-1.3.2.jar` | `needs-loader` | ran; 4 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `takeitout-1.1.28-26.3.jar` | `needs-loader` | ran; 20 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `titlechanger-1.4.8+mc26.3.jar` | `needs-loader` | ran; 1 plan(s) skipped, at least half aimed at the game (`target-outside-jar`) |
| `257814-8894326.jar` | `native-gap` | refused by an uncaught exception: `uncaught exception java/lang/AssertionError (at team/creative/creativecore/mixi…` |
| `342584-8921261.jar` | `native-gap` | refused by an uncaught exception: `uncaught exception java/lang/NullPointerException: Tried to register a listener…` |
| `531761-8889669.jar` | `native-gap` | refused by an uncaught exception: `uncaught exception java/lang/IllegalArgumentException: Invalid version range '[…` |
| `623764-8914234.jar` | `native-gap` | refused by an uncaught exception: `uncaught exception java/lang/IllegalStateException: Could not determine mod loa…` |
| `Legacy4J-26.3-1.9.1-fabric.jar` | `native-gap` | refused while decoding metadata: `Metadata("malformed java/lang/Boolean marker (at wily/legacy/init/LegacyRegistr…` |
| `betterstats-5.6.0-beta.2+fn-26.3.jar` | `native-gap` | refused by an uncaught exception: `uncaught exception java/lang/IllegalStateException: Unexpected subclass com/t...` |
| `betterstats-5.7.0+fn-26.3.jar` | `native-gap` | refused by an uncaught exception: `uncaught exception java/lang/IllegalStateException: Unexpected subclass com/the…` |
| `clientsort-fabric-3.104.1+26.3.jar` | `native-gap` | refused while decoding metadata: `Metadata("expected a native collection marker, got null (call java/util/Map#key…` |
| `collective-26.3.0-8.42.jar` | `native-gap` | refused by an uncaught exception: `uncaught exception java/lang/NullPointerException: Tried to register a listener…` |
| `collective-26.3.0-8.46.jar` | `native-gap` | refused by an uncaught exception: `uncaught exception java/lang/NullPointerException: Tried to register a listener…` |
| `kreno_fpatcher-26.2.0+26.1.jar` | `native-gap` | refused by an uncaught exception: `uncaught exception java/lang/SecurityException: Unsupported mod detected: bukki…` |
| `main-menu-credits-1.4.0+26.2-universal.jar` | `native-gap` | refused while decoding metadata: `Metadata("java/util/ServiceLoader has no explicitly seeded providers fo...` |
| `moderner-beta-fabric-5.0.0-alpha.4+26.3.jar` | `native-gap` | refused while decoding metadata: `Metadata("array index 0 out of bounds (at mod/bluestaggo/modernerbeta/...` |
| `overlaylib-0.12.1+26.3.jar` | `native-gap` | refused by an uncaught exception: `uncaught exception java/lang/NullPointerException: reflection on a null Class (…` |
| `pickupnotifications-fabric-3.0.1+26.3.jar` | `native-gap` | refused by an uncaught exception: `uncaught exception java/lang/NullPointerException: codec (at me/roundaro...` |

_12 more row(s) are in the ledger, not this table._
<!-- java-mod-ledger:end -->

After a ledger run, regenerate the block without touching a jar set again:

```sh
python3 tools/java_mod_server_ledger.py --from tools/real_mod_fixtures/LAST_SERVER_LEDGER_223.txt \
  --docs docs/java-mod-compat-status.md
```

`--table` prints the same block without writing it, and `--table-limit N` sets how many per-jar rows it keeps.

### Tier coverage over the whole corpus

Every jar in both corpora gets exactly one tier. `native-ok` means the dedicated server runs it, or the
server role has nothing to run (client-only and library jars). `needs-loader` means native execution cannot
carry it but a real Fabric loader launch of its pack lists it; the row also says whether that launch reached
`Done (`. `native-gap` means neither tier carries it, with the cause. The block below is generated from the
two committed server ledgers and the two committed loader proof ledgers:

<!-- java-mod-coverage:start -->
_Generated by `tools/hybrid_pack.py --coverage --docs docs/java-mod-compat-status.md` from `LAST_SERVER_LEDGER_223.txt`, `LAST_SERVER_LEDGER_500.txt`, `LAST_LOADER_PROOF_223.txt`, `LAST_LOADER_PROOF_500.txt`; edit the tool, not this block._

**599 unique jars (723 rows across 2 corpora): 446 `native-ok`, 150 `needs-loader`, 0 `native-gap`, 3 out of scope.**

| Corpus | Jars | `native-ok` | `needs-loader` | `native-gap` | out of scope |
|---|---:|---:|---:|---:|---:|
| `corpus-xl` | 223 | 164 | 59 | 0 | 0 |
| `mega` | 500 | 382 | 115 | 0 | 3 |
| union (SHA-256) | 599 | 446 | 150 | 0 | 3 |

**Loader proof.** All 174 of 174 `needs-loader` rows are demonstrated by a real Fabric loader launch: 169 reached `Done (`, 5 were listed by a launch that did not boot (client-only code in the server role), and 0 have no loader evidence.

**Native refusals absorbed by the loader tier (80 rows).** They are `native-gap` in the server ledger; the real loader lists them, so the tier coverage counts them as `needs-loader`.

| Cause | Rows |
|---|---:|
| runtime | 46 |
| metadata | 30 |
| other | 2 |
| declaration | 1 |
| verifier | 1 |

**Out of scope (3).** Built for another game version, so no 26.3 tier can carry them; they belong out of the corpus.

| Jar | Corpus | Why |
|---|---|---|
| `1bb71c0df6-coroutil-fabric-1.21.1-1.3.8.jar` | `mega` | built for another game version (requires minecraft 1.21.x); it cannot run on 26.3 |
| `949fa8a29a-show-me-your-skin-2.0.3+26.2.jar` | `mega` | built for another game version (requires minecraft >=26.2 <26.3); it cannot run on 26.3 |
| `c90f03eedd-noisium-fabric-2.8.5+mc26.2-pre-2.jar` | `mega` | built for another game version (requires minecraft ~26.2-); it cannot run on 26.3 |

**Listed but not booted (5).** The loader discovered these jars, but the launch did not reach `Done (`.

| Jar | Corpus | Loader id |
|---|---|---|
| `02d17d2f1c-Axiom-6.1.5-for-MC26.3.jar` | `mega` | `axiom` |
| `3c1a43daba-sway-2.4.5-fabric+26.3.jar` | `mega` | `sway` |
| `4ed60c0212-combat-hitboxes-1.0.11.jar` | `mega` | `combat-hitboxes` |
| `80a0fc2ac1-CreativeCore_FABRIC_v2.14.16_mc26.3.jar` | `mega` | `creativecore` |
| `812809eb6f-Axiom-6.1.3-for-MC26.3.jar` | `mega` | `axiom` |
<!-- java-mod-coverage:end -->

A row whose reason says `requires minecraft …` is out of scope: the jar was built for another game version
(1.21, 26.2), so no 26.3 tier can carry it and it belongs out of the corpus rather than in a compatibility
fix. The five `listed but not booted` rows are client-only code that declares a server entrypoint: the loader
discovers and weaves them, then their `main` touches `net.minecraft.client` and the dedicated server stops.
They are `needs-loader` because the loader half is what a client pack uses; proving them in a client launch
needs a client game jar, which the server corpus does not carry.

Regenerate it without starting a game:

```sh
python3 tools/hybrid_pack.py --coverage \
  --corpus corpus-xl=~/horizon-corpus/corpus-xl --corpus mega=~/horizon-corpus/mega \
  --ledger tools/real_mod_fixtures/LAST_SERVER_LEDGER_223.txt \
  --ledger tools/real_mod_fixtures/LAST_SERVER_LEDGER_500.txt \
  --loader-ledger tools/real_mod_fixtures/LAST_LOADER_PROOF_223.txt \
  --loader-ledger tools/real_mod_fixtures/LAST_LOADER_PROOF_500.txt \
  --docs docs/java-mod-compat-status.md
```

`--measure-proof` fills the proof ledgers: for every row without a listing it launches the real loader with
the jar's declared dependency closure, splits a batch that cannot boot, and retries with each `--proof-loader`.
`--strict` exits non-zero while a `native-gap` row remains.

### Hybrid sessions: a native half and a loader half

The label decides where a jar runs: `native-ok` in one `horizon-client --java-mod` process, `needs-loader` in
`horizon-jvm --real-loader --launch-game`, and `native-gap` in neither unless `--gap-to-loader`.
`tools/hybrid_pack.py` partitions a pack by these ledgers (falling back to SHA-256 when the pack's filenames differ
from the corpus), closes each half over declared `depends`, checks the pack's own declarations, and `--measure` runs
both halves. The design and the two launch commands are in
[java-mod-compat.md](java-mod-compat.md#hybrid-sessions-a-native-half-and-a-loader-half).

Measured 6 November 2026 on the 112-jar Helix pack, matched to the two committed ledgers by content hash:

| Half | Jars | Entrypoints executed | Mixin plans applied | Wall |
|---|---:|---:|---:|---:|
| native-only (native half) | 75 | 18 | 0 (976 skipped) | 2.1 s |
| hybrid — native half | 75 | 18 | 0 (976 skipped) | 1.7 s |
| hybrid — loader half | 33 | 67 mods loaded | 346 (45 configs, 200 targets, 0 failed) | timed out at 180 s |

The partition routes 21 `needs-loader` jars to the loader, plus the 10 `native-ok` libraries their closure shares with
the native half and 2 `native-gap` jars (`worldedit`, `xaeroworldmap`) that loader jars depend on; 7 `native-gap` and
7 hash-unmatched jars stay out. The native half is unchanged and still applies no game mixins. The loader half loads
67 mods and applies 346 mixins with 0 failures, but does not reach `Done (` within the runner's 180 s bound: it stops
while transforming `NaturalSpawner` (architectury). The correction that moved 45 wide-corpus jars to `needs-loader`
therefore makes the default hybrid a larger, slower session that is not yet a working one. The static check also
reports 5 `cross-partition-dependency` conflicts: `litematica-printer` needs `litematica` and `malilib`,
`pickupnotifier` needs `puzzleslib`, `stackrefill` needs `collective`, and `wakes` needs `baguettelib`, all of which
run in the other half.

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

Measured on 2026-11-06 (16 CPUs) over the same 500 jars, one flat-world dedicated run per point, `Done (N.NNNs)!`
and peak RSS; a 0-jar run takes 2.2-2.4 s, so "jar work" is wall time over that same binary's baseline. Both rows
use **one binary**, and only `HORIZON_WORKER_THREADS` differs (`1` against the machine's 16): the difference is the
bootstrap pool, not another change landing between runs.

| 500 jars | Wall to `Done` | Jar work | Peak RSS | Started |
|---|---:|---:|---:|---:|
| serial (`HORIZON_WORKER_THREADS=1`) | 15.90 s | 13.5 s | 828 MiB | 191 |
| parallel (default pool) | **10.46 s** | 8.2 s | 859 MiB | 191 |
| bootstrap batch, serial | 10.22 s | — | — | 191 |
| bootstrap batch, parallel | **5.14 s** | — | — | 191 |

The batch is most of the run and the pool takes 5.1 s off it (10.22 -> 5.14); the rest of startup is unchanged, so
the whole run drops 5.4 s. Peak RSS grows 31 MiB, because up to eight jars are parsed and bootstrapped at once.
**191 of the 500 started in that profile run**; the 19:02 binary's ledger starts 197 and skips 7,742 plans, so the
profile remains a parser/loader measurement, not a claim that 500 effective server mods run.

Caveats: the host was loaded (absolute fps is depressed and noisy); vanilla ran `-Xmx2G` and RSS is measured at
`Done`; the client ran offscreen with `maxFps=260` because the default 120 fps cap hides all frame differences.

### Bootstrap content cache (AOT)

What a jar declares depends on its bytes, this build's engine and its role, so a jar that started once is stored and
replayed instead of parsed and executed again (`game/java_cache.rs`). Measured 6 November 2026 (16 CPUs, load
average about 10) on the first 100 of the 197 jars that start natively from the 500-jar corpus: one flat-world
dedicated run per point, one game folder, `HORIZON_HOME` and world reused, the cache cleared before each cold run,
eight cold and eight warm starts alternating.

| Start | Wall to `Done` (8 runs) | Median | Peak RSS (median) | Reused |
|---|---|---:|---:|---:|
| cold (empty cache) | 2.88–4.34 s | 3.29 s | 821 MiB | 0 of 100 |
| warm (filled cache) | 2.82–3.73 s | 3.45 s | 722 MiB | 86 of 100 |

On this loaded host the wall difference is inside the run-to-run noise: the eight pairs average 0.02 s in the
warm start's favour and range from 0.58 s against it to 0.85 s for it (an earlier, quieter run of the same suite
measured 2.48 s cold against 2.41 s warm). What is stable is memory: the warm start's median peak RSS is ~100 MiB
lower because 86 jars are replayed rather than parsed at once, and its `java-runtime.json` is byte-identical to the
cold run's. The fourteen jars not reused declare payload channels, whose handlers need the bootstrapped host rather
than rows; the cache held 86 entries in 436 KiB.

## Known gaps

1. **Game-class mixins do not apply.** The load weave only rewrites classes inside the jar; a plan whose target is a
   Minecraft class is skipped. That is every plan that matters (0 woven; 2,944 and 7,742 skipped), so `ran` means
   "entrypoint executed, mixins skipped, vanilla server reached `Done`", not "the mod works". The opt-in hook tier
   binds two game-class sites (`player_tick`, `item_picked_up`) but the game does not fire it yet.
2. **Refusals.** 27 of 121 wide and 68 of 265 mega jars that declare a server entrypoint still fail (22%/26%). The
   causes are named above and in the ledgers. The raw log line is bounded at 160 characters, so a human sees a clipped
   cause; the ledger recovers the skip class from the jar's own metadata.
3. **Nested and non-server entrypoints.** Nested JiJ/JarJar jars are probed but never bootstrapped in-game (`preLaunch`
   is not run either); two mega jars now fail the interpreter's 128-nested-jar-frame bound. The Kotlin entrypoint
   adapter runs, but an adapter the runtime still does not model is refused.
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
   jars that started); `java-runtime.json` carries each jar's woven/skipped counts and the first game-class or
   unsupported skip reason, so the launcher's runtime panel can label a jar `needs-loader`, but it does not list the
   plans behind that reason.
8. **NeoForge.** Native metadata and `@Mod` bootstrap work. The real FML lifecycle is a bounded, opt-in child
   process (`horizon-jvm --real-loader`) used as a preflight on the integrated server, not the path a mod's code
   takes; the dedicated role does not preflight yet, there is no in-game loader selection, and no automatic
   intermediary-to-official remapping.
9. **The numbers are narrow.** Both corpora are filename-sorted slices of 26.3 Fabric jars, the client run is
   offscreen with no mod drawing, and the efficiency suite has no steady-state or playable-session figure.

## Planned work

1. **Make mixins count.** Fire the opt-in game-hook registrations from the game's own hook points, widen the
   hand-checked site table from its two rows, and map the remaining game-class plans onto hook points (or provide the
   target classes through the loader path). Gate: `woven` or `run_game_hook` covered by a real-mod ledger, and every
   `native` label backed by zero skipped plans.
2. **Close metadata refusals.** Bounded native collection/stream markers, `ServiceLoader`, exact `Integer.parseInt`,
   `String.split`/`Map#keySet` receivers, `Gson` appendables and tighter `fabric.mod.json` bounds. Gate: metadata
   refusals 38 → 0 across both corpora.
3. **Close runtime refusals.** Entrypoint static-init failures (null config/listener phase, reflection on a null
   class), the `StackUnderflow` verifier gap, the nested-jar-frame bound and duplicate declarations. Gate: refused share
   of in-scope server jars below 5% (currently 22%/26%).
4. **Finish content.** Block entities, recipes and ingredients in the content model; data-component and attachment
   declaration tables; guests consume the applied manifest in command/content lookups; then mod hot reload.
5. **Client surface.** Compile the façade client families (HUD, keys, tooltips, screens, renderers, models) into the
   runtime and drive them from the game; measure with rendering on and a curated pack, not offscreen.
6. **NeoForge path.** Finish the FML path: give the dedicated role the integrated server's opt-in preflight, then
   in-process loader selection, with remap readiness checked against a real NeoForge 26.3 server.
7. **Diagnostics.** Populate the runtime report's weave counts and first skip reason, include refused jars in
   `server-status`, stop clipping the refusal cause, and surface a per-mod compatibility row (with cache clear) in the
   launcher.
8. **Keep the corpus honest.** Refresh `tools/real_mod_fixtures/LAST_SERVER_LEDGER_{223,500}.txt` per release, fail
   a run when `ran` regresses or a refused row loses its cause, and extend the fixture workflow (it deliberately never
   downloads third-party jars).
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
  --binary target/fast/horizon-client --native-binary target/fast/horizon-native-java \
  --resources .local/horizon-client/26.3 --timeout 60 --json --output ledger.txt > ledger.json

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
