//! Local-only graphics trial workloads driven through the game's control queue.

use std::fs;
use std::io::BufWriter;
use std::path::Path;
use std::sync::mpsc::RecvTimeoutError;
use std::thread;
use std::time::{Duration, Instant};

use horizon_bench::{Frames, Report};
use horizon_core::position::BlockPos;
use horizon_world::registry::BlockStateId;
use serde_json::{Value, json};

use super::{
    Control, SCENES, fly, fps_metrics, frames_of, hold_scene, load_scene, ms, near, poll,
    timing_metrics, wait_loaded,
};

const PACK_PROBE: &str = "benchmark-fixture pack";
const WEATHER_PROBE: &str = "benchmark-fixture weather";
const READY: Duration = Duration::from_secs(15);

pub const AUTOTUNE_PACK_ID: &str = "file/horizon-autotune";
pub const AUTOTUNE_PACK_BLOCK: &str = "minecraft:purple_glazed_terracotta";
const PACK_TEXTURES: u32 = 16;
const TEXTURE_SIZE: u32 = 64;
const TEXTURE_FRAMES: u32 = 4;

pub(super) const fn repeats(thorough: bool) -> u32 {
    if thorough { 2 } else { 1 }
}

fn sprite(index: u32) -> String {
    format!("block/horizon_autotune_{index}")
}

/// Builds the trial pack in an isolated worker game directory. Select it by
/// [`AUTOTUNE_PACK_ID`] in that directory's options before starting the worker.
pub fn prepare_autotune_pack(game_dir: &Path) -> Result<(), String> {
    let root = game_dir.join("resourcepacks/horizon-autotune");
    let assets = root.join("assets/minecraft");
    let write =
        |path: &Path, value: Value| fs::write(path, value.to_string()).map_err(|e| e.to_string());
    for dir in ["blockstates", "models/block", "textures/block"] {
        fs::create_dir_all(assets.join(dir)).map_err(|e| e.to_string())?;
    }
    let (major, minor) = horizon_core::version::RESOURCE_PACK_FORMAT;
    write(
        &root.join("pack.mcmeta"),
        json!({"pack": {
            "description": "Horizon",
            "min_format": [major, minor], "max_format": [major, minor],
        }}),
    )?;
    write(
        &assets.join("blockstates/purple_glazed_terracotta.json"),
        json!({
            "variants": {"": {"model": "minecraft:block/horizon_autotune"}}
        }),
    )?;

    // A grid of extruded tiles, each with its own animated sprite. The scene
    // exposes their top and side faces rather than just loading unused textures.
    let mut elements = Vec::with_capacity(PACK_TEXTURES as usize);
    let mut textures = serde_json::Map::new();
    textures.insert("particle".into(), json!(format!("minecraft:{}", sprite(0))));
    let mut pixels =
        Vec::with_capacity((TEXTURE_FRAMES * TEXTURE_SIZE * TEXTURE_SIZE * 4) as usize);
    for index in 0..PACK_TEXTURES {
        let name = sprite(index);
        let key = format!("tile{index}");
        textures.insert(key.clone(), json!(format!("minecraft:{name}")));
        let texture = format!("#{key}");
        let faces: serde_json::Map<_, _> = ["up", "north", "south", "east", "west"]
            .into_iter()
            .map(|face| {
                (
                    face.to_owned(),
                    json!({"texture": texture, "uv": [0, 0, 16, 16]}),
                )
            })
            .collect();
        let (x, z) = (4 * (index % 4), 4 * (index / 4));
        elements.push(json!({
            "from": [x, 0, z], "to": [x + 3, 4 + index % 7, z + 3], "faces": faces
        }));

        pixels.clear();
        for frame in 0..TEXTURE_FRAMES {
            for y in 0..TEXTURE_SIZE {
                for x in 0..TEXTURE_SIZE {
                    let stripe = ((x / 4) ^ (y / 4) ^ frame) & 1;
                    pixels.extend([
                        (17 * index + 31 * frame) as u8,
                        (4 * x + 19 * index) as u8,
                        (3 * y + 64 * stripe + 13 * frame) as u8,
                        255,
                    ]);
                }
            }
        }
        let file = fs::File::create(assets.join(format!("textures/{name}.png")))
            .map_err(|e| e.to_string())?;
        let mut png = png::Encoder::new(
            BufWriter::new(file),
            TEXTURE_SIZE,
            TEXTURE_SIZE * TEXTURE_FRAMES,
        );
        png.set_color(png::ColorType::Rgba);
        png.set_depth(png::BitDepth::Eight);
        let mut writer = png.write_header().map_err(|e| e.to_string())?;
        writer
            .write_image_data(&pixels)
            .map_err(|e| e.to_string())?;
        writer.finish().map_err(|e| e.to_string())?;
        write(
            &assets.join(format!("textures/{name}.png.mcmeta")),
            json!({"animation": {"frametime": 2}}),
        )?;
    }
    write(
        &assets.join("models/block/horizon_autotune.json"),
        json!({"textures": textures, "elements": elements}),
    )
}

/// Verifies the live baked block model names all of the pack's animated sprite
/// layers; finding a folder on disk would not detect asset-loading fallbacks.
pub fn autotune_pack_loaded(render: &crate::assets::RenderAssets, state: BlockStateId) -> bool {
    let Ok(models) = render.model_catalog.collect(state, BlockPos::new(0, 0, 0)) else {
        return false;
    };
    (0..PACK_TEXTURES).all(|index| {
        let Some(&layer) = render
            .sprite_layers
            .get(&format!("minecraft:{}", sprite(index)))
        else {
            return false;
        };
        models
            .iter()
            .any(|(_, model)| model.faces.iter().any(|face| face.layer == layer))
            && render.animations.iter().any(|animation| {
                animation.layer == layer && animation.frame_count() == TEXTURE_FRAMES as usize
            })
    })
}

pub(super) fn check_pack(control: &Control) -> Result<(), String> {
    if control.call(PACK_PROBE)?["pack_loaded"] == true {
        Ok(())
    } else {
        Err(format!(
            "the benchmark resource pack was not applied: select {AUTOTUNE_PACK_ID} in the worker's game directory"
        ))
    }
}

pub(super) fn measure(
    control: &Control,
    thorough: bool,
    report: &mut Report,
    steady: &mut Vec<(Instant, Instant)>,
) -> Result<(), String> {
    control.call("time 6000")?;
    weather(control, false)?;
    let seconds = if thorough { 5 } else { 2 };
    let count = repeats(thorough);
    let (_, start, end, pitch) = SCENES[1]; // The existing benchmark's seed-1234 forest.
    let phase = "dense-terrain";
    load_scene(control, phase, start, end, pitch, report)?;
    let mut frames = Vec::with_capacity(count as usize);
    for repeat in 0..count {
        let mut sample = fly(
            control,
            phase,
            start,
            end,
            f64::from(seconds),
            pitch,
            steady,
        )?;
        sample.repeat = repeat;
        frames.push(sample);
    }
    record(report, phase, frames);

    // Stay above the same forest columns so fixture setup does not dominate
    // every trial with another full view load. Separate heights keep earlier
    // workloads out of the view of later scenes.
    let phase = "entities";
    pose(control, 222.0, 24.0)?;
    for row in 0..6 {
        for col in 0..12 {
            let kind = ["pig", "cow", "sheep"][(row + col) % 3];
            let x = -1108.25 + col as f64 * 1.5;
            let z = -371.5 + row as f64 * 2.0;
            control.call(&format!("command summon minecraft:{kind} {x} 215.5 {z} {{NoAI:1b,NoGravity:1b,PersistenceRequired:1b}}"))?;
        }
    }
    wait_for(phase, || {
        let entities = control.call("entities 32")?;
        Ok(entities["entities"].as_array().is_some_and(|list| {
            list.iter()
                .filter(|entity| {
                    entity["pos"][1] == 215.5
                        && matches!(
                            entity["type"].as_str(),
                            Some("minecraft:pig" | "minecraft:cow" | "minecraft:sheep")
                        )
                })
                .count()
                == 72
        }))
    })?;
    record(
        report,
        phase,
        stationary(control, phase, seconds, count, steady)?,
    );

    let phase = "block-entities";
    pose(control, 195.0, 24.0)?;
    let before = control.call("status")?["block_entities"]
        .as_u64()
        .ok_or("missing block entity count")?;
    control.call(
        "command fill -1107 188 -372 -1094 188 -361 minecraft:chest[facing=north,type=single] strict",
    )?;
    wait_for(phase, || {
        Ok(control.call("status")?["block_entities"]
            .as_u64()
            .is_some_and(|n| n >= before + 168))
    })?;
    record(
        report,
        phase,
        stationary(control, phase, seconds, count, steady)?,
    );

    let phase = "resource-pack";
    let camera = pose(control, 156.0, 24.0)?;
    let placed = Instant::now();
    control.call(&format!(
        "command fill -1107 149 -372 -1094 149 -357 {AUTOTUNE_PACK_BLOCK} strict"
    ))?;
    wait_for(phase, || {
        Ok(
            control.call("blocks -1107 149 -372 -1094 149 -357")?["counts"][AUTOTUNE_PACK_BLOCK]
                == 224,
        )
    })?;
    let loaded = wait_loaded(control, near(camera))?;
    check_pack(control)?;
    report.metric(
        "chunks.resource-pack.load_ms",
        &[ms(loaded.saturating_duration_since(placed))],
    );
    record(
        report,
        phase,
        stationary(control, phase, seconds, count, steady)?,
    );

    let phase = "ui";
    pose(control, 120.0, 16.0)?;
    if control.call("open-inventory")?["screen"] != "creative" {
        return Err("the creative inventory did not open over the world".into());
    }
    let frames = stationary(control, phase, seconds, count, steady)?;
    let status = control.call("status")?;
    if status["screen"] != "creative"
        || status["renderer"]["drawn_sections"].as_u64().unwrap_or(0) == 0
    {
        return Err("the creative inventory did not draw over terrain".into());
    }
    control.call("key escape tap")?;
    record(report, phase, frames);

    let phase = "particles-weather";
    pose(control, 120.0, 24.0)?;
    weather(control, true)?;
    let mut frames = Vec::with_capacity(count as usize);
    for repeat in 0..count {
        frames.push(sample_weather(control, phase, seconds, repeat, steady)?);
    }
    record(report, phase, frames);
    weather(control, false)?;

    let phase = "far-terrain";
    let mut frames = Vec::with_capacity(count as usize);
    let mut sampled_tiles = Vec::with_capacity(count as usize);
    // Different, untouched corridors so the second pass cannot simply replay
    // the first one's cached far tiles. Off still measures the same camera workload.
    for (repeat, (start, end)) in [
        ([560.0, 280.0, 860.0], [1072.0, 280.0, 860.0]),
        ([-4096.0, 280.0, -4096.0], [-4096.0, 280.0, -3584.0]),
    ]
    .into_iter()
    .take(count as usize)
    .enumerate()
    {
        let load_name = if repeat == 0 {
            phase
        } else {
            "far-terrain-repeat"
        };
        load_scene(control, load_name, start, end, 25.0, report)?;
        let status = control.call("status")?;
        let distance = status["lod"]["effective_distance"]
            .as_u64()
            .ok_or("missing far terrain distance")?;
        let near = status["render_distance"]
            .as_u64()
            .ok_or("missing render distance")?;
        let mut sampled = 0;
        if distance > near {
            wait_for(phase, || {
                let status = control.call("status")?;
                let lod = &status["lod"];
                sampled = lod["sampled_target_tiles"]
                    .as_u64()
                    .ok_or("missing far terrain sample count")?;
                Ok(lod["source"] == "integrated" && sampled > 0)
            })?;
        }
        let mut sample = fly(control, phase, start, end, f64::from(seconds), 25.0, steady)?;
        sample.repeat = repeat as u32;
        frames.push(sample);
        sampled_tiles.push(sampled as f64);
    }
    report.metric("workload.far-terrain.sampled_tiles", &sampled_tiles);
    record(report, phase, frames);
    Ok(())
}

fn pose(control: &Control, y: f64, pitch: f64) -> Result<[f64; 3], String> {
    let start = [-1100.5, y, -385.5];
    hold_scene(control, start, [-1100.5, y, -360.0], pitch)?;
    poll(control, READY, |status| {
        near(start)(&status["player"]["position"])
    })?;
    Ok(start)
}

fn wait_for(phase: &str, mut ready: impl FnMut() -> Result<bool, String>) -> Result<(), String> {
    let until = Instant::now() + READY;
    while Instant::now() < until {
        if ready()? {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(format!(
        "{phase}: fixture was not ready after {} s",
        READY.as_secs()
    ))
}

fn weather(control: &Control, rain: bool) -> Result<(), String> {
    let mode = if rain { "rain" } else { "clear" };
    control.call(&format!("command weather {mode} 600s"))?;
    wait_for("particles-weather", || {
        let level = control.call(WEATHER_PROBE)?["rain_level"]
            .as_f64()
            .ok_or("missing rain level")?;
        Ok(if rain { level > 0.8 } else { level < 0.1 })
    })
}

fn particle_burst(control: &Control) -> Result<(), String> {
    const COUNT: u64 = 96;
    let response = control
        .call("benchmark-fixture particle campfire_signal_smoke -1100.5 117 -373 3 1 4 96")?;
    let queued = response["queued"]
        .as_u64()
        .ok_or("missing queued smoke count")?;
    if queued != COUNT {
        return Err(format!(
            "the particle workload queued {queued} smoke particles, expected {COUNT}"
        ));
    }
    Ok(())
}

fn sample_weather(
    control: &Control,
    phase: &str,
    seconds: u32,
    repeat: u32,
    steady: &mut Vec<(Instant, Instant)>,
) -> Result<Frames, String> {
    particle_burst(control)?;
    let line = format!("bench {seconds} raw");
    let begin = Instant::now();
    let response = control.send(&line)?;
    let until = begin + Duration::from_secs(u64::from(seconds)) + READY;
    let reply = loop {
        match response.recv_timeout(Duration::from_millis(500)) {
            Ok(reply) => break Control::decode(&line, &reply)?,
            Err(RecvTimeoutError::Timeout) if Instant::now() < until => particle_burst(control)?,
            Err(_) => return Err(format!("{line}: no reply")),
        }
    };
    steady.push((begin, Instant::now()));
    frames_of(&reply, phase, repeat)
}

fn stationary(
    control: &Control,
    phase: &str,
    seconds: u32,
    count: u32,
    steady: &mut Vec<(Instant, Instant)>,
) -> Result<Vec<Frames>, String> {
    let line = format!("bench {seconds} raw");
    (0..count)
        .map(|repeat| {
            let begin = Instant::now();
            let reply = control.call(&line)?;
            steady.push((begin, Instant::now()));
            frames_of(&reply, phase, repeat)
        })
        .collect()
}

fn record(report: &mut Report, phase: &str, frames: Vec<Frames>) {
    fps_metrics(report, phase, &frames);
    timing_metrics(report, phase, &frames);
    report.frames.extend(frames);
}
