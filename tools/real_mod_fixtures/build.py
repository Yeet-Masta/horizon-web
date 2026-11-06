#!/usr/bin/env python3
"""Build small deterministic Fabric, NeoForge, and Forge fixture jars.

Only Java sources live in the repository.  The command writes generated jars and a
sha256 manifest below ``--output`` (normally a temporary directory used by tests).
"""
from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import subprocess
import tempfile
import zipfile
from pathlib import Path


ROOT = Path(__file__).resolve().parent


def _write(root: Path, files: dict[str, str]) -> list[Path]:
    paths = []
    for name, body in sorted(files.items()):
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(body, encoding="utf-8")
        paths.append(path)
    return paths


def _compile(source: dict[str, str], work: Path) -> Path:
    src, classes = work / "src", work / "classes"
    paths = _write(src, source)
    classes.mkdir()
    javac = shutil.which("javac")
    if not javac:
        raise RuntimeError("javac is required to build the real-mod fixtures")
    command = [javac, "-source", "8", "-target", "8", "-d", str(classes), *map(str, paths)]
    subprocess.run(command, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    return classes


def _jar(path: Path, classes: Path, resources: dict[str, bytes | str]) -> None:
    """Create a stable zip/jar with sorted entries and a fixed DOS timestamp."""
    jar = shutil.which("jar")
    if not jar:
        raise RuntimeError("jar is required to build the real-mod fixtures")
    subprocess.run(
        [jar, "--create", "--file", str(path), "--date=1980-01-01T00:00:02+00:00", "-C", str(classes), "."],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    entries: dict[str, bytes] = {}
    for file in sorted(classes.rglob("*.class")):
        entries[file.relative_to(classes).as_posix()] = file.read_bytes()
    for name, value in resources.items():
        entries[name] = value.encode() if isinstance(value, str) else value
    normalized = path.with_suffix(path.suffix + ".normalized")
    with zipfile.ZipFile(normalized, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for name in sorted(entries):
            info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o644 << 16
            archive.writestr(info, entries[name])
    normalized.replace(path)


FABRIC_SOURCES = {
    "net/fabricmc/api/ModInitializer.java": "package net.fabricmc.api; public interface ModInitializer { void onInitialize(); }",
    "net/fabricmc/fabric/api/networking/v1/ServerPlayNetworking.java": (
        "package net.fabricmc.fabric.api.networking.v1; "
        "public final class ServerPlayNetworking { "
        "public interface PlayPayloadHandler<T> { void receive(T payload, Object context); } "
        "public static <T> boolean registerGlobalReceiver(Object type, PlayPayloadHandler<T> handler) { return true; } }"
    ),
    "net/minecraft/resources/Identifier.java": (
        "package net.minecraft.resources; public final class Identifier { "
        "public static Identifier fromNamespaceAndPath(String n, String p) { return new Identifier(); } }"
    ),
    "demo/fabric/Payload.java": "package demo.fabric; public final class Payload { public static int seen; }",
    "demo/fabric/Mod.java": (
        "package demo.fabric; import net.fabricmc.api.ModInitializer; "
        "import net.fabricmc.fabric.api.networking.v1.ServerPlayNetworking; "
        "import net.minecraft.resources.Identifier; "
        "public final class Mod implements ModInitializer { public void onInitialize() { "
        "Payload.seen++; ServerPlayNetworking.<Object>registerGlobalReceiver(Identifier.fromNamespaceAndPath(\"fixture\", \"payload\"), this::receive); } "
        "private void receive(Object payload, Object context) { Payload.seen++; } }"
    ),
    "org/spongepowered/asm/mixin/Mixin.java": (
        "package org.spongepowered.asm.mixin; import java.lang.annotation.*; "
        "@Retention(RetentionPolicy.CLASS) @Target(ElementType.TYPE) public @interface Mixin { Class<?>[] value() default {}; }"
    ),
    "org/spongepowered/asm/mixin/injection/At.java": (
        "package org.spongepowered.asm.mixin.injection; import java.lang.annotation.*; "
        "@Retention(RetentionPolicy.RUNTIME) @Target(ElementType.METHOD) public @interface At { String value(); }"
    ),
    "org/spongepowered/asm/mixin/injection/Inject.java": (
        "package org.spongepowered.asm.mixin.injection; import java.lang.annotation.*; "
        "@Retention(RetentionPolicy.RUNTIME) @Target(ElementType.METHOD) public @interface Inject { String method(); At at(); }"
    ),
    "demo/fabric/Target.java": "package demo.fabric; public class Target { public void tick() {} }",
    "demo/fabric/FixtureMixin.java": (
        "package demo.fabric; import org.spongepowered.asm.mixin.Mixin; "
        "import org.spongepowered.asm.mixin.injection.*; @Mixin(Target.class) public class FixtureMixin { "
        "@Inject(method=\"tick\", at=@At(\"HEAD\")) private void head() {} }"
    ),
}

NEO_COMMON = {
    "net/neoforged/fml/common/Mod.java": (
        "package net.neoforged.fml.common; import java.lang.annotation.*; "
        "@Retention(RetentionPolicy.RUNTIME) @Target(ElementType.TYPE) public @interface Mod { String value(); }"
    ),
    "net/neoforged/bus/api/IEventBus.java": "package net.neoforged.bus.api; public interface IEventBus {}",
    "net/neoforged/fml/javafmlmod/FMLJavaModLoadingContext.java": (
        "package net.neoforged.fml.javafmlmod; import net.neoforged.bus.api.IEventBus; "
        "public final class FMLJavaModLoadingContext { public static FMLJavaModLoadingContext get() { return new FMLJavaModLoadingContext(); } "
        "public IEventBus getModEventBus() { return null; } }"
    ),
    "net/neoforged/neoforge/registries/DeferredRegister.java": (
        "package net.neoforged.neoforge.registries; import net.neoforged.bus.api.IEventBus; "
        "public final class DeferredRegister<T> { public static <T> DeferredRegister<T> create(Object r, String id) { return new DeferredRegister<>(); } "
        "public void register(IEventBus bus) {} public void register(String id, Object value) {} }"
    ),
    "demo/neo/NeoMod.java": (
        "package demo.neo; import net.neoforged.fml.common.Mod; import net.neoforged.bus.api.IEventBus; "
        "import net.neoforged.fml.javafmlmod.FMLJavaModLoadingContext; import net.neoforged.neoforge.registries.DeferredRegister; "
        "@Mod(\"native_fixture_neo\") public final class NeoMod { public NeoMod(IEventBus bus) { "
        "DeferredRegister.create(\"items\", \"native_fixture_neo\").register(bus); "
        "DeferredRegister.create(\"items\", \"native_fixture_neo\").register(\"crystal\", null); } "
        "public static IEventBus eventBus() { return FMLJavaModLoadingContext.get().getModEventBus(); } }"
    ),
}

FORGE_COMMON = {
    "net/minecraftforge/fml/common/Mod.java": (
        "package net.minecraftforge.fml.common; import java.lang.annotation.*; "
        "@Retention(RetentionPolicy.RUNTIME) @Target(ElementType.TYPE) public @interface Mod { String value(); }"
    ),
    "net/minecraftforge/network/simple/SimpleChannel.java": (
        "package net.minecraftforge.network.simple; public final class SimpleChannel { "
        "public SimpleChannel registerMessage(int id, Class<?> type, Object encoder, Object decoder, Object handler) { return this; } }"
    ),
    "demo/forge/ForgeMod.java": (
        "package demo.forge; import net.minecraftforge.fml.common.Mod; import net.minecraftforge.network.simple.SimpleChannel; "
        "@Mod(\"native_fixture_forge\") public final class ForgeMod { public ForgeMod() { "
        "new SimpleChannel().registerMessage(0, Object.class, null, null, null); } }"
    ),
}


def build(output: Path) -> dict[str, dict[str, str]]:
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="horizon-real-fixtures-") as tmp:
        work = Path(tmp)
        fabric_classes = _compile(FABRIC_SOURCES, work / "fabric")
        nested_classes = _compile({"demo/nested/Nested.java": "package demo.nested; public final class Nested { public void onInitialize() {} }"}, work / "nested")
        nested = work / "nested.jar"
        _jar(nested, nested_classes, {"fabric.mod.json": '{"schemaVersion":1,"id":"nested_fixture","version":"1.0","entrypoints":{"main":"demo.nested.Nested"}}'})
        _jar(output / "fabric-fixture.jar", fabric_classes, {
            "fabric.mod.json": '{"schemaVersion":1,"id":"native_fixture_fabric","version":"1.0.0","entrypoints":{"main":"demo.fabric.Mod"},"depends":{"minecraft":">=26.3","fabric-api":">=0.100"},"jars":[{"file":"META-INF/jars/nested.jar"}],"mixins":["native_fixture.mixins.json"]}',
            "native_fixture.mixins.json": '{"required":true,"package":"demo.fabric","compatibilityLevel":"JAVA_8","mixins":["FixtureMixin"]}',
            "META-INF/jars/nested.jar": nested.read_bytes(),
        })
        neo_classes = _compile(NEO_COMMON, work / "neo")
        _jar(output / "neoforge-fixture.jar", neo_classes, {"META-INF/neoforge.mods.toml": 'modLoader="javafml"\nloaderVersion="[1,)"\nlicense="MIT"\n\n[[mods]]\nmodId="native_fixture_neo"\nversion="1.0.0"\n'} )
        forge_classes = _compile(FORGE_COMMON, work / "forge")
        _jar(output / "forge-fixture.jar", forge_classes, {"META-INF/mods.toml": 'modLoader="javafml"\nloaderVersion="[1,)"\nlicense="MIT"\n\n[[mods]]\nmodId="native_fixture_forge"\nversion="1.0.0"\n'} )
    manifest = {}
    for jar in sorted(output.glob("*.jar")):
        manifest[jar.name] = {"sha256": hashlib.sha256(jar.read_bytes()).hexdigest(), "size": jar.stat().st_size}
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return manifest


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        manifest = build(args.output)
    except RuntimeError as error:
        if "javac" in str(error):
            print("skipping: javac is unavailable")
            return 0
        raise
    print(json.dumps(manifest, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
