//! A "Fabric API bootstrap" fixture: real Fabric API mods register listeners on
//! `Event` objects (`ServerLifecycleEvents.SERVER_STARTING`,
//! `CommandRegistrationCallback.EVENT`) and ask `FabricLoader.isModLoaded` which
//! siblings are present.  This compiles that shape from Java source at test time.
//!
//! API stubs are compiled onto a separate classpath and deliberately *not*
//! packaged in the jar, mirroring a real install where Fabric API is a host
//! surface: every API call crosses `Host` and is observable on `StdHost`.

use horizon_native_java::{EventDispatch, HostInvocation, NativeError, NativeJar, StdHost, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};
use zip::write::SimpleFileOptions;

const FABRIC_LOADER: &str = "net/fabricmc/loader/api/FabricLoader";
const FABRIC_EVENT: &str = "net/fabricmc/fabric/api/event/Event";
const SERVER_LIFECYCLE: &str = "net/fabricmc/fabric/api/event/lifecycle/v1/ServerLifecycleEvents";
const COMMAND_CALLBACK: &str = "net/fabricmc/fabric/api/command/v2/CommandRegistrationCallback";

/// A Java source tree for [`build_jar`]: relative path => source text.
macro_rules! sources {
    ($($path:literal => $body:expr),* $(,)?) => {{
        let mut map: BTreeMap<&str, &str> = BTreeMap::new();
        $(map.insert($path, $body);)*
        map
    }};
}

fn javac() -> PathBuf {
    std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into())
}

fn files_under(root: &Path) -> Vec<PathBuf> {
    fs::read_dir(root)
        .unwrap()
        .flat_map(|entry| {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files_under(&path)
            } else {
                vec![path]
            }
        })
        .collect()
}

fn scratch_root(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-fabric-api-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&root);
    root
}

fn write_sources(root: &Path, tree: &BTreeMap<&str, &str>) -> Vec<PathBuf> {
    tree.iter()
        .map(|(relative, body)| {
            let path = root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, body).unwrap();
            path
        })
        .collect()
}

fn compile(out: &Path, classpath: Option<&Path>, sources: &[PathBuf], label: &str) {
    fs::create_dir_all(out).unwrap();
    let mut command = Command::new(javac());
    command.arg("--release").arg("17").arg("-d").arg(out);
    if let Some(classpath) = classpath {
        command.arg("-cp").arg(classpath);
    }
    let status = command.args(sources).status().unwrap();
    assert!(status.success(), "javac failed for {label}");
}

/// Compile API stubs onto a classpath, compile the mod against them, and package
/// only the mod's own class files plus its loader metadata.
fn build_jar(
    name: &str,
    stubs: &BTreeMap<&str, &str>,
    sources: &BTreeMap<&str, &str>,
    metadata: &[u8],
) -> (PathBuf, PathBuf) {
    let root = scratch_root(name);
    let stub_classes = root.join("stub-classes");
    let classes = root.join("classes");
    let stub_paths = write_sources(&root.join("stub-src"), stubs);
    compile(&stub_classes, None, &stub_paths, name);
    let paths = write_sources(&root.join("src"), sources);
    compile(&classes, Some(&stub_classes), &paths, name);

    let jar_path = root.join(format!("{name}.jar"));
    let mut zip = zip::ZipWriter::new(fs::File::create(&jar_path).unwrap());
    zip.start_file("fabric.mod.json", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(metadata).unwrap();
    for class in files_under(&classes) {
        let entry = class
            .strip_prefix(&classes)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        zip.start_file(entry, SimpleFileOptions::default()).unwrap();
        zip.write_all(&fs::read(class).unwrap()).unwrap();
    }
    zip.finish().unwrap();
    (root, jar_path)
}

fn calls_to<'a>(host: &'a StdHost, owner: &str, name: &str) -> Vec<&'a HostInvocation> {
    host.invocations()
        .iter()
        .filter(|invocation| invocation.method.owner == owner && invocation.method.name == name)
        .collect()
}

/// The callback target a registration recorded in its `@indy:` marker
/// (`@indy:<class>#<method>:<index>:<target>:<captures>`).
fn indy_target(invocation: &HostInvocation) -> String {
    let marker = invocation
        .args
        .iter()
        .filter_map(|value| match value {
            Value::Ref(Some(marker)) => Some(marker.as_str()),
            _ => None,
        })
        .find(|marker| marker.starts_with("@indy:"))
        .unwrap_or_else(|| panic!("no invokedynamic marker in {invocation:?}"));
    // The target never contains `:`, so the captures after it cannot shift it.
    marker
        .split(':')
        .nth(3)
        .unwrap_or_else(|| panic!("malformed @indy marker {marker}"))
        .to_owned()
}

/// The Fabric API surface a real mod compiles against, kept host-side.
fn fabric_api_stubs() -> BTreeMap<&'static str, &'static str> {
    sources! {
        "net/fabricmc/api/ModInitializer.java" =>
            "package net.fabricmc.api; public interface ModInitializer { void onInitialize(); }",
        "net/fabricmc/api/ClientModInitializer.java" =>
            "package net.fabricmc.api; public interface ClientModInitializer { void onInitializeClient(); }",
        "net/fabricmc/loader/api/FabricLoader.java" => r#"package net.fabricmc.loader.api;
public final class FabricLoader {
    public static FabricLoader getInstance() { return new FabricLoader(); }
    public boolean isModLoaded(String id) { return false; }
}"#,
        "net/fabricmc/fabric/api/event/Event.java" => r#"package net.fabricmc.fabric.api.event;
    public interface Event<T> { void register(T listener); }"#,
        "net/fabricmc/fabric/api/event/EventFactory.java" => r#"package net.fabricmc.fabric.api.event;
import java.util.function.Function;
public final class EventFactory {
    public static <T> Event<T> createArrayBacked(Class<T> type, Function<T[], T> factory) { return null; }
}"#,
        "net/minecraft/server/MinecraftServer.java" =>
            "package net.minecraft.server; public class MinecraftServer { }",
        "net/minecraft/commands/CommandSourceStack.java" =>
            "package net.minecraft.commands; public class CommandSourceStack { }",
        "net/minecraft/commands/CommandBuildContext.java" =>
            "package net.minecraft.commands; public interface CommandBuildContext { }",
        "com/mojang/brigadier/CommandDispatcher.java" =>
            "package com.mojang.brigadier; public class CommandDispatcher<S> { }",
        "net/minecraft/commands/Commands.java" => r#"package net.minecraft.commands;
    public final class Commands { public enum CommandSelection { ALL, DEDICATED, INTEGRATED } }"#,
        "net/fabricmc/fabric/api/event/lifecycle/v1/ServerLifecycleEvents.java" => r#"package net.fabricmc.fabric.api.event.lifecycle.v1;
import net.fabricmc.fabric.api.event.Event;
import net.fabricmc.fabric.api.event.EventFactory;
import net.minecraft.server.MinecraftServer;
public final class ServerLifecycleEvents {
    @FunctionalInterface
    public interface ServerStarting { void onServerStarting(MinecraftServer server); }
    public static final Event<ServerStarting> SERVER_STARTING = EventFactory.createArrayBacked(
            ServerStarting.class, listeners -> server -> {
                for (ServerStarting listener : listeners) { listener.onServerStarting(server); }
            });
}"#,
        "net/fabricmc/fabric/api/command/v2/CommandRegistrationCallback.java" => r#"package net.fabricmc.fabric.api.command.v2;
import com.mojang.brigadier.CommandDispatcher;
import net.fabricmc.fabric.api.event.Event;
import net.fabricmc.fabric.api.event.EventFactory;
import net.minecraft.commands.CommandBuildContext;
import net.minecraft.commands.CommandSourceStack;
import net.minecraft.commands.Commands;
@FunctionalInterface
public interface CommandRegistrationCallback {
    Event<CommandRegistrationCallback> EVENT = EventFactory.createArrayBacked(
            CommandRegistrationCallback.class, listeners -> (dispatcher, buildContext, environment) -> {
                for (CommandRegistrationCallback listener : listeners) { listener.register(dispatcher, buildContext, environment); }
            });
    void register(CommandDispatcher<CommandSourceStack> dispatcher, CommandBuildContext buildContext,
            Commands.CommandSelection environment);
}"#,
    }
}

/// The mod itself: exactly the bootstrap a small real Fabric API mod runs.
fn fabric_api_mod() -> BTreeMap<&'static str, &'static str> {
    sources! {
        "demo/FabricApiMod.java" => r#"
package demo;
import com.mojang.brigadier.CommandDispatcher;
import net.fabricmc.api.ModInitializer;
import net.fabricmc.fabric.api.command.v2.CommandRegistrationCallback;
import net.fabricmc.fabric.api.event.lifecycle.v1.ServerLifecycleEvents;
import net.fabricmc.loader.api.FabricLoader;
import net.minecraft.commands.CommandBuildContext;
import net.minecraft.commands.CommandSourceStack;
import net.minecraft.commands.Commands;
import net.minecraft.server.MinecraftServer;
public final class FabricApiMod implements ModInitializer {
    @Override
    public void onInitialize() {
        ServerLifecycleEvents.SERVER_STARTING.register(this::onServerStarting);
        CommandRegistrationCallback.EVENT.register(this::onRegisterCommands);
        if (FabricLoader.getInstance().isModLoaded("fabric-api")) {
            System.getenv("fabric-api");
        }
    }
    private void onServerStarting(MinecraftServer server) { System.getenv("server-starting"); }
    private void onRegisterCommands(CommandDispatcher<CommandSourceStack> dispatcher,
            CommandBuildContext buildContext, Commands.CommandSelection environment) {
        System.getenv("command-registration");
    }
}
"#,
    }
}

fn build_fabric_api_mod() -> (PathBuf, NativeJar) {
    let (root, jar_path) = build_jar(
        "bootstrap",
        &fabric_api_stubs(),
        &fabric_api_mod(),
        br#"{"id":"fixture-fabric-api","version":"1.0","depends":{"fabric-api":"*"},"entrypoints":{"main":"demo.FabricApiMod"}}"#,
    );
    (root, NativeJar::from_path(&jar_path).unwrap())
}

#[test]
fn fabric_api_bootstrap_registers_both_listeners_in_declaration_order() {
    let (root, jar) = build_fabric_api_mod();
    assert_eq!(jar.fabric_id.as_deref(), Some("fixture-fabric-api"));
    assert_eq!(jar.required_mods, ["fabric-api"]);

    let entry = jar.discover_entrypoint("main").unwrap();
    assert_eq!(entry.class, "demo.FabricApiMod");
    assert_eq!(entry.method, None);

    // `seed_host` exposes the jar's own id plus its declared dependencies, which
    // is what `isModLoaded("fabric-api")` answers from.
    let mut host = jar.seed_host(StdHost::new());
    let report = jar
        .execute(&entry.class, entry.method.as_deref(), "main", &mut host)
        .unwrap();
    assert_eq!(report.method, "onInitialize");
    assert!(report.calls > 0, "the initializer must execute bytecode");

    // Host call counts by owner/method: two static event handles, one
    // `Event.register` per listener, one loader query, and the branch body.
    let count = |owner: &str, name: &str| calls_to(&host, owner, name).len();
    assert_eq!(count(SERVER_LIFECYCLE, "SERVER_STARTING"), 1);
    assert_eq!(count(COMMAND_CALLBACK, "EVENT"), 1);
    assert_eq!(count(FABRIC_EVENT, "register"), 2);
    assert_eq!(count(FABRIC_LOADER, "getInstance"), 1);
    assert_eq!(count(FABRIC_LOADER, "isModLoaded"), 1);
    assert_eq!(count("java/lang/System", "getenv"), 1);
    assert_eq!(
        calls_to(&host, "java/lang/System", "getenv")[0].args,
        [Value::Ref(Some("fabric-api".into()))]
    );
    // The static field itself crosses the host as an `Event`-typed read.
    let lifecycle = calls_to(&host, SERVER_LIFECYCLE, "SERVER_STARTING");
    assert_eq!(lifecycle[0].method.descriptor, format!("L{FABRIC_EVENT};"));

    // Generic `Event<T>.register(T)` erases to `(Ljava/lang/Object;)V` at the
    // call site, which is the shape the listener record carries.
    for register in calls_to(&host, FABRIC_EVENT, "register") {
        assert_eq!(register.method.descriptor, "(Ljava/lang/Object;)V");
        assert_eq!(register.receiver, Some(Value::Ref(None)));
    }

    // Both listeners are recorded, in declaration order, with the resolved
    // callback target.  They are bound instance method references, so the
    // declaring instance travels as a capture.
    let targets: Vec<String> = host.listener_registrations().map(indy_target).collect();
    assert_eq!(
        targets,
        [
            "demo/FabricApiMod#onServerStarting(Lnet/minecraft/server/MinecraftServer;)V",
            "demo/FabricApiMod#onRegisterCommands(Lcom/mojang/brigadier/CommandDispatcher;\
             Lnet/minecraft/commands/CommandBuildContext;\
             Lnet/minecraft/commands/Commands$CommandSelection;)V",
        ]
    );

    // This bootstrap registers listeners only: no registry entries, payload
    // channels, or event-bus posts.  A loader adapter can rely on that.
    assert!(host.registrations().is_empty());
    assert!(host.payload_registrations().is_empty());
    assert!(host.posted_events().is_empty());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn fabric_api_server_starting_event_dispatches_the_registered_listener() {
    let (root, jar) = build_fabric_api_mod();
    let entry = jar.discover_entrypoint("main").unwrap();
    let mut host = jar.seed_host(StdHost::new());
    jar.execute(&entry.class, entry.method.as_deref(), "main", &mut host)
        .unwrap();

    let event = Value::Ref(Some("event/server-starting".into()));
    let dispatches = host.dispatch_event(event.clone());
    assert_eq!(dispatches.len(), 2, "both Fabric listeners are recorded");
    assert_eq!(dispatches[0].event, event);
    assert_eq!(dispatches[1].event, event);

    // Selection follows registration order and keeps the indy targets intact.
    let dispatched: Vec<String> = dispatches
        .iter()
        .map(|dispatch: &EventDispatch| indy_target(&dispatch.listener))
        .collect();
    let recorded: Vec<String> = host.listener_registrations().map(indy_target).collect();
    assert_eq!(dispatched, recorded);

    // The server-lifecycle callback takes one argument, so the host can run it:
    // the event is appended after the captured receiver.
    let before = calls_to(&host, "java/lang/System", "getenv").len();
    assert_eq!(
        jar.execute_dispatch(&dispatches[0], &mut host).unwrap(),
        Value::Void
    );
    let getenv = calls_to(&host, "java/lang/System", "getenv");
    assert_eq!(getenv.len(), before + 1);
    assert_eq!(
        getenv.last().unwrap().args,
        [Value::Ref(Some("server-starting".into()))]
    );

    // A cancelled marker still selects `Event#register` listeners: the Fabric
    // path never models cancellation, so selection must not drop them silently.
    let cancelled = host.dispatch_event(Value::Ref(Some("event/cancelled/server-starting".into())));
    assert_eq!(cancelled.len(), 2);

    // GAP: `CommandRegistrationCallback` takes three arguments (dispatcher,
    // build context, environment) and `execute_dispatch` supplies one event
    // value.  The runtime reports the arity instead of panicking or inventing
    // the missing objects.
    let error = jar.execute_dispatch(&dispatches[1], &mut host).unwrap_err();
    match error {
        NativeError::Metadata(message) => assert!(
            message.contains("expected 3 arguments, got 1"),
            "unexpected dispatch error: {message}"
        ),
        other => panic!("expected a bounded arity error, got {other:?}"),
    }

    let _ = fs::remove_dir_all(root);
}

#[test]
fn client_only_initializer_executed_as_main_reports_the_missing_entrypoint() {
    let sources = sources! {
        "demo/ClientOnlyMod.java" => r#"
package demo;
import net.fabricmc.api.ClientModInitializer;
public final class ClientOnlyMod implements ClientModInitializer {
    public void onInitializeClient() { System.getenv("client-only"); }
}
"#,
    };
    let (root, jar_path) = build_jar(
        "client-only",
        &fabric_api_stubs(),
        &sources,
        br#"{"id":"fixture-client-only","version":"1.0","entrypoints":{"client":"demo.ClientOnlyMod"}}"#,
    );
    let jar = NativeJar::from_path(&jar_path).unwrap();

    // Discovery is honest about the side: there is no `main` entrypoint at all.
    assert!(jar.discover_entrypoint("main").is_none());
    assert!(jar.discover_entrypoints("main").is_empty());
    let client = jar.discover_entrypoint("client").unwrap();
    assert_eq!(client.class, "demo.ClientOnlyMod");

    // Running it on the wrong side is a bounded error naming the entrypoint
    // method the `main` lifecycle would have called.
    let mut host = jar.seed_host(StdHost::new());
    let error = jar
        .execute(&client.class, None, "main", &mut host)
        .unwrap_err();
    match error {
        NativeError::MissingMethod(message) => assert!(
            message.contains("demo/ClientOnlyMod#onInitialize"),
            "missing entrypoint must be named, got {message}"
        ),
        other => panic!("expected MissingMethod, got {other:?}"),
    }

    // Positive control: the same class runs on its declared side.
    let report = jar
        .execute(&client.class, None, "client", &mut host)
        .unwrap();
    assert_eq!(report.method, "onInitializeClient");

    let _ = fs::remove_dir_all(root);
}
