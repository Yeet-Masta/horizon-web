//! A compiled mod registers a payload receiver and the native dispatcher calls
//! it with the received frame's bounded marker.

use horizon_native_java::{
    NativeJar, StdHost, Value, dispatch_payload_frame, dispatch_payload_frame_with,
};
use std::{collections::BTreeMap, fs, io::Write, path::Path, process::Command};
use zip::write::SimpleFileOptions;

fn javac() -> std::path::PathBuf {
    std::env::var_os("JAVA_HOME")
        .map(|home| std::path::PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into())
}

fn files_under(root: &Path) -> Vec<std::path::PathBuf> {
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

fn build_mod(root: &Path) -> std::path::PathBuf {
    let stubs = root.join("stubs");
    let classes = root.join("classes");
    let src = root.join("src");
    fs::create_dir_all(&classes).unwrap();
    let sources = BTreeMap::from([
        (
            "net/fabricmc/api/ModInitializer.java",
            "package net.fabricmc.api; public interface ModInitializer { void onInitialize(); }",
        ),
        (
            "net/minecraft/resources/Identifier.java",
            "package net.minecraft.resources; public final class Identifier { public static Identifier fromNamespaceAndPath(String ns, String path) { return new Identifier(); } public String toString() { return \"demo:frame\"; } }",
        ),
        (
            "net/minecraft/network/codec/StreamCodec.java",
            "package net.minecraft.network.codec; public interface StreamCodec<B, V> {}",
        ),
        (
            "net/minecraft/network/protocol/common/custom/CustomPacketPayload.java",
            "package net.minecraft.network.protocol.common.custom; import net.minecraft.network.codec.StreamCodec; public interface CustomPacketPayload { final class Type<T extends CustomPacketPayload> { public Type(net.minecraft.resources.Identifier id, StreamCodec<?, T> codec) {} } }",
        ),
        (
            "net/fabricmc/fabric/api/networking/v1/ServerPlayNetworking.java",
            "package net.fabricmc.fabric.api.networking.v1; import net.minecraft.network.protocol.common.custom.CustomPacketPayload; public final class ServerPlayNetworking { public interface PlayPayloadHandler<T extends CustomPacketPayload> { void receive(T payload, Object context); } public static <T extends CustomPacketPayload> boolean registerGlobalReceiver(CustomPacketPayload.Type<T> type, PlayPayloadHandler<T> handler) { return true; } }",
        ),
        (
            "demo/Probe.java",
            "package demo; public final class Probe { public static void seen(String value) {} }",
        ),
        (
            "demo/Mod.java",
            "package demo; import net.fabricmc.api.ModInitializer; import net.fabricmc.fabric.api.networking.v1.ServerPlayNetworking; import net.minecraft.network.protocol.common.custom.CustomPacketPayload; import net.minecraft.resources.Identifier; public final class Mod implements ModInitializer { public void onInitialize() { CustomPacketPayload.Type<CustomPacketPayload> type = new CustomPacketPayload.Type<>(Identifier.fromNamespaceAndPath(\"demo\", \"frame\"), null); ServerPlayNetworking.registerGlobalReceiver(type, this::handle); } private void handle(CustomPacketPayload payload, Object context) { Probe.seen(payload.toString()); } }",
        ),
    ]);
    let mut paths = Vec::new();
    for (relative, body) in sources {
        let path = src.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
        paths.push(path);
    }
    let status = Command::new(javac())
        .arg("--release")
        .arg("17")
        .arg("-d")
        .arg(&classes)
        .args(&paths)
        .status()
        .unwrap();
    assert!(status.success(), "javac failed");

    let jar = root.join("payload-dispatch.jar");
    let mut zip = zip::ZipWriter::new(fs::File::create(&jar).unwrap());
    zip.start_file("fabric.mod.json", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(
        br#"{"id":"payload-dispatch","version":"1.0","entrypoints":{"main":"demo.Mod"}}"#,
    )
    .unwrap();
    for class in files_under(&classes) {
        let entry = class
            .strip_prefix(&classes)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        // Keep the mod's own classes only: every stub call crosses the host.
        if !entry.starts_with("demo/Mod") {
            continue;
        }
        zip.start_file(entry, SimpleFileOptions::default()).unwrap();
        zip.write_all(&fs::read(class).unwrap()).unwrap();
    }
    zip.finish().unwrap();
    let _ = stubs;
    jar
}

#[test]
fn a_received_frame_calls_the_registered_handler() {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-payload-dispatch-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&root);
    let jar_path = build_mod(&root);
    let jar = NativeJar::from_path(&jar_path).unwrap();
    let entry = jar.discover_entrypoint("main").unwrap();
    let mut host = jar.seed_host(StdHost::new());
    jar.execute(&entry.class, entry.method.as_deref(), "main", &mut host)
        .unwrap();
    let registrations = host.payload_registrations().to_vec();
    assert_eq!(registrations.len(), 1);
    assert_eq!(registrations[0].direction, "playToServer");
    let dispatched =
        dispatch_payload_frame(&jar, &mut host, &registrations, "demo:frame", b"hi").unwrap();
    assert!(dispatched, "the handler should have been invoked");
    let seen = host
        .invocations()
        .iter()
        .find(|call| call.method.owner == "demo/Probe" && call.method.name == "seen")
        .expect("Probe.seen was called");
    let Value::Ref(Some(text)) = &seen.args[0] else {
        panic!("Probe.seen got {:?}", seen.args[0]);
    };
    // The handler observed the channel and the frame's first bytes.
    assert!(text.contains("demo:frame"), "{text}");
    assert!(text.contains("6869"), "{text}");

    // A decoded value (a supported codec's string) reaches the same handler.
    let dispatched = dispatch_payload_frame_with(
        &jar,
        &mut host,
        &registrations,
        "demo:frame",
        Value::Ref(Some("decoded-text".into())),
        Value::Ref(Some("payload-context/demo:frame".into())),
    )
    .unwrap();
    assert!(dispatched);
    let seen = host
        .invocations()
        .iter()
        .rev()
        .find(|call| call.method.owner == "demo/Probe" && call.method.name == "seen")
        .expect("Probe.seen was called again");
    assert_eq!(seen.args[0], Value::Ref(Some("decoded-text".into())));
    let _ = fs::remove_dir_all(root);
}
