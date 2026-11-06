# The Java interpreter as a VM (design)

Status: design, 6 November 2026, on the `java-interpreter` branch of the web port. It plans how
`crates/horizon-native-java` grows from a bounded bytecode probe into a VM that runs a Fabric or
NeoForge 26.3 mod's own code, its bundled libraries, the parts of the JDK it uses and the libraries
Minecraft ships about as faithfully as HotSpot, in the native game and in the browser. Minecraft's
own classes stay on the `Host` side: Horizon is the game. Read [java-mod-compat.md](java-mod-compat.md)
for the runtime's place in Horizon and its tiers, and [web/README.md](../web/README.md) for the browser.

## Where it starts: the baseline

Measured on this branch (the PR 10409 head `a5cd52ab8` merged into `web`) with the conformance
harness, `python3 tools/java_conformance.py` (HotSpot 25 is the oracle; every program also runs on
the interpreter natively and in wasm32-wasip1 under Node):

| | today |
|---|---|
| `cargo test -p horizon-native-java` | all pass (lib 228, probe 8, 46 fixture files) |
| horizon-client Java tests (`game::java_*`, 88) | all pass |
| wasm32 / wasm64 build of the crate (atomics, unwinding) | builds |
| conformance, 182 scored programs | 51 pass (28%), the same 51 in wasm; native and wasm agree on all 222 |
| mod-shaped jars (6) | 1 right: HashMap order, enum `name()`, a captured lambda, a bundled resource and NeoForge's suppliers go wrong |
| 24 real 26.3 mods (14 Fabric, 10 NeoForge), both roles | 42/53 entrypoints complete; 156 registrations, 0 typed; 2 jars crash the process (Rust stack overflow) |
| benchmarks | 1 of 6 gives the right answer: 1.9 s natively, 6.3 s in wasm, HotSpot `-Xint` 52 ms |

The failures have one cause: objects are string markers. There is no heap, so a value the runtime does
not model becomes a marker that later calls cannot read (`name()` answers `""`, a map iterates in
insertion order, `Collectors` see an empty stream), and a call no model covers stops the run.
"Complete" today often means "ran with wrong values".

## Decisions

### 1. The JDK: native Rust for the core, real bytecode for every library

The browser has no JDK, and OpenJDK and Mojang's files can't be shipped. Two ways remain:

- **Run real `java.base` bytecode** from the player's install, as rusty-jvm does. The numbers against
  it: a hello world loads 430 `java.base` classes and our typical programs 730 to 1040; HotSpot itself
  needs 59 ms to boot them interpreted without CDS (40 ms with), with an assembly interpreter far faster
  than one in Rust, and rusty-jvm needs about 800 ms. The 50 ms budget for a whole mod in wasm leaves
  no room for that. It also needs about 300 natives (rusty-jvm: 291, 42 of them `Unsafe`), the
  `java.lang.invoke` machinery that spins hidden classes, a `java.base` of 25.6 MB (5.9 MB brotli) in
  every player's prepared resources, and a JRE in every install.
- **Implement the core JDK in Rust**, as natives of the VM: `java.lang` (Object, String, StringBuilder,
  the wrappers and their caches, Math/StrictMath, System, Thread, Throwable and the VM's exceptions,
  Enum, Record, Class), `java.util` (the collections with Java's exact algorithms and iteration order,
  Optional, Objects, Arrays, Collections, Random, UUID, regex, Formatter), `java.util.function`,
  streams and collectors, the basics of `java.util.concurrent`, `java.io` streams, readers and writers in
  memory, `java.nio` buffers and charsets, `java.time` durations and instants, `java.math`, reflection and
  `java.lang.invoke`. No boot sequence, no class files to parse, natives run at Rust speed, and nothing
  to ship but code.

**Decision: the second.** Everything that is not the JDK runs as real bytecode: the mod, its
jar-in-jar libraries, and the libraries Minecraft ships (Gson, Guava, fastutil, DataFixerUpper,
Brigadier, JOML, the slf4j API), which `horizon-prepare` copies from the player's install into
`.local/` and the web build reads from the prepared resources, as it reads the rest. The cost is the
long tail of the JDK: a call into a JDK method we have not written stops the run with a diagnostic
naming it (never a silent default), the harness counts them, and the corpus decides what comes next.
A fallback that loads a single leaf class (say `java.text.DecimalFormat`) from a prepared `java.base`
subset stays possible later, for classes that touch no JDK internals; it is not part of this plan.

The JDK classes library bytecode extends (AbstractList, AbstractMap, InputStream, Reader, Number,
Thread, Exception...) are native classes that bytecode can subclass: a native method may call a
virtual method back into bytecode (AbstractCollection.toString calls `iterator()`), through a nested
run of the interpreter on the same thread.

### 2. Heap and values

- **Slots** are untyped 64-bit words: ints, floats, longs, doubles and references, as the bytecode says.
  A reference is an index into the heap arena plus one (0 is null). Long and double keep the JVM's two
  local slots, so local numbering is the class file's.
- **Objects** live in one arena: a header (class id, identity hash, lock word) and a body: field slots
  for a bytecode class, a typed vector for an array (`[I`, `[J`, `[F`, `[D`, `[B`, `[Z`, `[C`, `[S`, and
  references with their element class), or native data for a native class (a String's UTF-16 code
  units, kept Latin-1 when they fit, as the JDK does; a HashMap's table). A bytecode subclass of a
  native class has both. Arrays are bounds-checked and reference stores type-checked
  (`ArrayStoreException`).
- **Strings** are objects; literals and `intern()` go through one intern table, so `"a" == "a"` holds
  across classes.
- **Identity hash codes** come from a per-VM xorshift with a fixed seed, assigned on first use: the
  same execution gives the same hashes, on any machine.
- **Host objects** (an `Item.Properties` the host made, a registry handle) are heap objects of a host
  class carrying the host's marker; the marker-to-object table keeps one object per marker, so identity
  survives a round trip through the host.
- **At the Host boundary** values stay what `Host` speaks today: strings become `Value::Ref(Some(text))`,
  host objects their marker, VM objects a marker minted for them (`vm/<class>#<id>`, pinned while the
  host holds it), boxed primitives the primitive where the descriptor says `Object`. StdHost's
  Minecraft and loader models (registries, property builders, event buses, payloads) keep working
  unchanged; its JDK models are no longer reached by the VM.

### 3. Classes

- **Class path**, in order: native JDK classes for `java.`, `javax.`, `jdk.` and `sun.` names we
  implement (an unknown `java.` class is `NoClassDefFoundError` with the name); the mod jar, then its
  nested jars (`META-INF/jars`, `META-INF/jarjar`); the prepared Minecraft libraries; then **host
  classes** for the host's namespaces (`net/minecraft/`, `com/mojang/blaze3d/`, `net/fabricmc/`,
  `net/neoforged/`, `org/spongepowered/`...): made on first reference, every member forwarding to `Host`.
  Jars are indexed by name and classes parsed only when first loaded (fastutil has 10,000 classes).
- **Linking**: superclass and interfaces resolved, field layout computed, vtables and itables built,
  default methods selected as JVMS 5.4.3.3 says (maximally-specific, `IncompatibleClassChangeError` and
  `AbstractMethodError` when it says so), nestmates honored for private access.
- **Initialization** follows JVMS 5.5: the triggers (new, static access, static call, reflection,
  subclass init, `Class.forName`), superclass first, superinterfaces with default methods,
  `ExceptionInInitializerError` then `NoClassDefFoundError`, and another green thread waiting on a class
  being initialized.
- **Mod classes extending host classes** (`MyBlock extends Block`, block entities, screens): the
  instance is a heap object with the subclass's fields and a host marker. `super(...)` and inherited
  methods go to `Host` with that marker as the receiver; overridden methods dispatch to bytecode; and
  the host calls an override on the object through a callback interface (section 7).

### 4. Execution

- **Pre-decoding**: a method is decoded once, on first call, into compact instructions: branch targets
  as instruction indices, constants materialized, `wide` and the switch tables unpacked. Field and method
  references are resolved on first execution and the instruction rewritten in place (quickening), which
  keeps JVMS's lazy resolution and its errors. A field access becomes a slot index, a call a method id,
  a vtable index, or an interface call with a monomorphic inline cache. Nothing allocates and nothing
  looks up a string per instruction.
- **Frames** are windows on one slot stack per thread (locals then operand stack), no allocation per
  call; the interpreter loop never recurses in Rust for a Java call.
- **Exceptions** are precise: exception tables by instruction, the VM's own `NullPointerException`
  (with JDK 14's helpful messages), `ArrayIndexOutOfBoundsException`, `ClassCastException`,
  `ArithmeticException`, `NegativeArraySizeException`, `ArrayStoreException`,
  `IllegalMonitorStateException` with HotSpot's messages, and stack traces from `LineNumberTable` at
  `fillInStackTrace`.

### 5. invokedynamic and method handles

The bootstraps javac emits are intrinsics: `LambdaMetafactory.metafactory`/`altMetafactory` make a
VM-internal lambda class (captures as fields, the interface method and bridges calling the implementation
handle; `Serializable` and marker interfaces honored), `StringConcatFactory` runs the recipe,
`ObjectMethods` gives records `toString`/`equals`/`hashCode`, `SwitchBootstraps.typeSwitch`/`enumSwitch`
and `ConstantBootstraps` do what the JDK does. Any other bootstrap (Kotlin's, a library's) runs as
bytecode over a native `java.lang.invoke`: direct handles from `MethodHandles.Lookup.find*`, `invoke`
and `invokeExact` with `asType` adaptation, the common combinators (bind, insert/drop arguments,
filters, `guardWithTest`), call sites, and `VarHandle` on fields and arrays.

### 6. Garbage collection

Mark and sweep, non-moving, over the arena with a free list. Roots: every thread's slot stack, scanned
conservatively (a word that is a live object's index keeps it alive: safe for a collector that does not
move objects, and still deterministic), statics, the intern table, class mirrors, objects the host has
pinned, and natives' own references. Objects are scanned precisely (field and element types are known).
A collection runs at a safe point (a call, a backward branch, an allocation in bytecode) once the
allocation budget is spent: bytes allocated since the last collection, counted from Java-visible sizes
only, so a collection happens at the same instruction on every machine, and weak references clear at
the same moment. Each mod's VM has a heap cap; an allocation past it, after a collection, throws
`OutOfMemoryError`.

### 7. Threads, time and callbacks

- **Green threads**, scheduled inside the interpreter: round-robin, switching every N instructions or
  when a thread blocks, so the interleaving depends only on the program. `Thread.start`/`join`/`sleep`,
  `synchronized` (a lock word, inflated to a monitor when contended or waited on), `wait`/`notify`,
  `LockSupport.park`, and over those `ExecutorService`, `ForkJoinPool.commonPool`, `CompletableFuture`
  and the `java.util.concurrent` locks and latches. Never `std::thread`, never a blocking wait: when an
  entrypoint returns while threads it started still run, they continue when the game pumps the VM on
  its own thread, within a budget.
- **Time** is virtual: `nanoTime` and `currentTimeMillis` advance with the instructions run, from an
  epoch the host gives (the same on server and client), and `sleep` advances the clock to the next
  wake-up when every thread waits. `new Random()`, `Map.of`'s salt, `ThreadLocalRandom` and
  `UUID.randomUUID` get seeds from the VM's own seeded generator.
- **Callbacks**: `Host` gains a method, with a default that forwards to today's, that receives a handle
  through which the host calls back into the VM: run a `Supplier` a mod gave a `DeferredRegister`, a
  listener for an event, an overridden method on a mod's block. The call runs on the calling thread as a
  nested run of the interpreter.

### 8. Reflection

One `Class` mirror per class (primitives, arrays, native and host classes included). `getDeclared*`
from the class file (native classes describe their members in their registration), `Field`/`Method`/
`Constructor` with `get`/`set`/`invoke`/`newInstance`, access checks off after `setAccessible`,
`InvocationTargetException`, annotations from `RuntimeVisibleAnnotations` as annotation objects with the
JDK's `toString`, `equals`, `hashCode` and defaults, records' components, enum constants, generic
signatures as `Type` objects (Gson's `TypeToken` needs them), `Class.forName` on the mod's class path,
and `java.lang.reflect.Proxy` classes made in the VM.

### 9. Mixins

The weaver is unchanged: load-time weaving rewrites the mod's own classes and the VM runs the result.
Mixins into Minecraft classes stay host hooks, as in PR 10409. The `execute_mixin_*` entry points,
which interpret mixin callbacks with their own hook tables, keep running on today's interpreter until
the hooks are ported; their tests stay as they are.

### 10. Limits and the sandbox

- The crate uses no `std::fs`, `std::net`, `std::process`, `std::thread` or `std::time`: `NativeJar`
  reads from bytes (`from_bytes`), and the file-opening `from_path` is kept for native callers only.
  Files, the network, the environment and processes are reachable only through `Host`, which refuses
  them by default (a `FileNotFoundException`, as for a missing file).
- Per-call limits: instructions (the run stops with an error, it is not a Java exception), frame depth
  (`StackOverflowError`), heap (`OutOfMemoryError`), and the depth of nested native-to-bytecode calls.
- A malformed class file is an error at load or at first use, never a panic or a hang: every decode
  step is checked, arithmetic wraps as Java's does, and a fuzz test mutates class files to keep it so.

### 11. The code, and moving onto it

The VM is new code in `src/vm/` (`heap`, `class`, `decode`, `interp`, `invoke`, `indy`, `thread`,
`reflect`, `host`, and the native library in `jdk/`), so lib.rs's 32,000 lines, which upstream keeps
changing, are touched only where a public entry point switches over. The steps, each a reviewable
commit with the harness numbers:

1. The VM runs `main` programs: the runner's `--engine vm`, conformance up from 28%.
2. The JDK core, reflection and threads: conformance to 90% and above, native and wasm identical.
3. `NativeJar::execute` and the payload dispatch run on the VM (the bounded runtime stays for the
   mixin execution paths), host classes and subclasses of them, callbacks, the library class path; the
   real-mod corpus and the mod-shaped jars before and after.
4. GC, limits, the fuzz test; the benchmarks to within 3x of HotSpot `-Xint`, and a typical mod's start
   in 50 ms or less in wasm.
5. The web: `from_bytes` through `horizon_platform::fs`, the payload handlers run outside
   `HANDLER_JARS`'s lock, `.jar` files dropped on the page go to `java-mods/`.

Tests that pin an artifact of the marker model (a host-call count that included JDK calls, a marker's
text) are updated in the step that changes them, each named in its commit.

## Open points

- `horizon_core::jmath` and `horizon_core::random` are the right sources for `Math` and `Random`, but
  depending on `horizon-core` brings `horizon-platform` into this crate; whether the WASI probe still
  links decides between the dependency and moving those two modules into a crate of their own.
- The prepared-library list (which jars `horizon-prepare` copies, and their size in the resources zip)
  is measured in step 3.
