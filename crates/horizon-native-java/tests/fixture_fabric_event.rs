//! A source-built Fabric fixture that crosses the native event registration
//! boundary with a real `invokedynamic` listener marker.

use horizon_native_java::{MethodRef, NativeJar, StdHost, Value};
use std::{fs, io::Write, process::Command};
use zip::write::SimpleFileOptions;

#[test]
fn source_built_fabric_fixture_registers_and_dispatches_listener() {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-fabric-event-fixture-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let src = root.join("src");
    let classes = root.join("classes");
    fs::create_dir_all(src.join("demo")).unwrap();
    fs::create_dir_all(src.join("net/fabricmc/fabric/api/event")).unwrap();
    fs::write(
        src.join("net/fabricmc/fabric/api/event/Event.java"),
        r#"
package net.fabricmc.fabric.api.event;
public interface Event<T> { void register(Listener<T> listener); }
"#,
    )
    .unwrap();
    fs::write(
        src.join("net/fabricmc/fabric/api/event/Listener.java"),
        r#"
package net.fabricmc.fabric.api.event;
@FunctionalInterface public interface Listener<T> { void accept(T value); }
"#,
    )
    .unwrap();
    fs::write(
        src.join("net/fabricmc/fabric/api/event/EventFactory.java"),
        r#"
package net.fabricmc.fabric.api.event;
import java.util.function.Function;
public final class EventFactory {
  public static <T> Event<T> createArrayBacked(Class<T> type, Function<Object[], T> factory) { return null; }
}
"#,
    )
    .unwrap();
    fs::write(
        src.join("demo/EventEntrypoint.java"),
        r#"
package demo;
import net.fabricmc.fabric.api.event.Event;
import net.fabricmc.fabric.api.event.EventFactory;
public final class EventEntrypoint {
  interface Listener { void accept(Object value); }
  public void onInitialize() {
    Event<Listener> event = EventFactory.createArrayBacked(Listener.class, null);
    event.register(value -> { });
  }
}
"#,
    )
    .unwrap();

    let javac = std::env::var_os("JAVA_HOME")
        .map(|home| std::path::PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into());
    fs::create_dir_all(&classes).unwrap();
    let sources = vec![
        src.join("net/fabricmc/fabric/api/event/Event.java"),
        src.join("net/fabricmc/fabric/api/event/Listener.java"),
        src.join("net/fabricmc/fabric/api/event/EventFactory.java"),
        src.join("demo/EventEntrypoint.java"),
    ];
    let status = Command::new(javac)
        .arg("--release")
        .arg("17")
        .arg("-d")
        .arg(&classes)
        .args(&sources)
        .status()
        .unwrap();
    assert!(status.success(), "javac failed");

    let jar_path = root.join("fabric-event.jar");
    let file = fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    zip.start_file("fabric.mod.json", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(
        br#"{"id":"fixture-fabric-event","version":"1.0","entrypoints":{"main":"demo.EventEntrypoint"}}"#,
    )
    .unwrap();
    let entry_class = classes.join("demo/EventEntrypoint.class");
    zip.start_file("demo/EventEntrypoint.class", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(&fs::read(entry_class).unwrap()).unwrap();
    zip.finish().unwrap();

    let jar = NativeJar::from_path(&jar_path).unwrap();
    let entry = jar.discover_entrypoint("main").unwrap();
    let mut host = StdHost::new();
    jar.execute(&entry.class, entry.method.as_deref(), "main", &mut host)
        .unwrap();
    assert_eq!(host.listener_registrations().count(), 1);

    let dispatches = host.dispatch_event(Value::Ref(Some("fixture-event".into())));
    assert_eq!(dispatches.len(), 1);
    assert_eq!(
        dispatches[0].listener.method.owner,
        "net/fabricmc/fabric/api/event/Event"
    );
    assert_eq!(
        dispatches[0].event,
        Value::Ref(Some("fixture-event".into()))
    );

    let event = MethodRef {
        owner: "net/fabricmc/fabric/api/event/EventFactory".into(),
        name: "createArrayBacked".into(),
        descriptor:
            "(Ljava/lang/Class;Ljava/util/function/Function;)Lnet/fabricmc/fabric/api/event/Event;"
                .into(),
    };
    assert!(host.calls().iter().any(|call| call == &event));
    let _ = fs::remove_dir_all(root);
}
