//! `Tags` read from a compound is a set, for every entity kind that keeps one.
//!
//! `Entity.load` keeps the tags in `Set<String> tags` and fills it with
//! `input.read("Tags", TAG_LIST_CODEC).ifPresent(this.tags::addAll)`
//! (Entity.java:286, :2230-2231), so a `Tags` list that repeats a name loads as
//! the set of its names: `/tag <targets> list` counts it once, and the
//! compound `/data get` reads and the save writes holds it once. Horizon keeps
//! the loaded compound of an entity that is not a mob raw and reads `Tags` out
//! of it, so the set has to be made where that compound is stored.
use crate::game::commands::Target;
use crate::game::{Audio, Game, Options, Progress, Settings, execution_role, load_resources};
use horizon_core::nbt::Tag;
use std::sync::Arc;

fn game() -> Game<'static> {
    let root = std::path::PathBuf::from(
        horizon_std::env::var_os("HORIZON_TEST_RESOURCES")
            .expect("set HORIZON_TEST_RESOURCES to prepared resources"),
    );
    let settings = Settings::default();
    let loaded = load_resources(
        horizon_assets::resource_source::ResourceRoot::directory(root.clone()),
        Vec::new(),
        None,
        &root,
        &settings,
        horizon_std::time::Instant::now(),
        false,
        execution_role::ExecutionRole::IntegratedServer,
        &Progress::new(),
        &mut |_, _| unreachable!(),
    )
    .unwrap();
    let mut options = Options::parse(std::iter::empty()).unwrap().unwrap();
    options.resources = root.clone();
    // A fixed name: no generated horizon-player.txt in the working directory.
    options.player_name = Some("TagLoad".into());
    options.execution_role = execution_role::ExecutionRole::IntegratedServer;
    let audio = Audio::silent(&loaded.pack.source);
    let res: &'static _ = Box::leak(Box::new(loaded.resources));
    let mut game = Game::new(
        res,
        loaded.pack,
        loaded.gui.unwrap(),
        options,
        settings,
        audio,
        None,
    )
    .unwrap();
    game.build_registries();
    game.set_allow_commands(true);
    game.world
        .chunks
        .insert((0, 0), Arc::new(horizon_client::chunk::Chunk::empty(0, 0)));
    game.block_ticks
        .add_empty_chunk(horizon_core::position::ChunkPos::new(0, 0))
        .unwrap();
    game
}

/// Runs `command`: whether it succeeded and what it said first.
fn run(game: &mut Game<'_>, command: &str) -> (bool, Option<String>) {
    game.chat_capture = Some(Vec::new());
    let ok = game.run_command(command);
    let said = game.chat_capture.take().unwrap_or_default();
    (ok, said.first().map(horizon_client::text::Text::plain))
}

/// The `Tags` of `t`'s saved compound, as `Entity.saveWithoutId` writes them.
fn saved_tags(game: &mut Game<'_>, t: Target) -> Vec<String> {
    match game
        .command_target_nbt(t)
        .and_then(|tag| tag.get("Tags").cloned())
    {
        Some(Tag::List(list)) => list
            .iter()
            .map(|t| match t {
                Tag::String(s) => s.to_string_lossy(),
                other => panic!("a tag that is not a string: {other:?}"),
            })
            .collect(),
        other => panic!("no Tags list in the saved compound: {other:?}"),
    }
}

/// A mob, an armour stand, an item frame, a falling block, an orb and an arrow,
/// each summoned with `Tags` that repeat a name: the set keeps one of it, in
/// all three views of the entity (`/tag list` reads `tags_of`; `/data get` and
/// the save read the compound).
#[test]
#[ignore = "daily"]
fn a_repeated_tag_loads_once_for_every_compound_horizon_keeps() {
    let mut game = game();
    // The item frame's extra data keeps the frame's item, which is not the
    // point here; `Facing:0b` is the down face it is summoned with.
    let kinds = [
        ("minecraft:pig", ""),
        ("minecraft:armor_stand", ""),
        (
            "minecraft:item_frame",
            ",Facing:0b,Item:{id:\"minecraft:stone\",count:1}",
        ),
        // `FallingBlockEntity` keeps its `Tags` in its saved compound only.
        ("minecraft:falling_block", ""),
        // `remember_tags`: kinds Horizon keeps no compound for.
        ("minecraft:experience_orb", ""),
        ("minecraft:arrow", ""),
    ];
    let mut found = Vec::new();
    for (index, (kind, extra)) in kinds.iter().enumerate() {
        let n = index as i32 + 1;
        let at = format!("{} 100 4.5", 4.5 + index as f64 * 4.0);
        assert!(
            game.run_command(&format!(
                "summon {kind} {at} {{UUID:[I;253,0,0,{n}],NoAI:1b,Motion:[0d,0d,0d],Tags:[\"b\",\"a\",\"a\",\"b\",\"c\"]{extra}}}"
            )),
            "{kind} did not spawn"
        );
        let target = game
            .target_with_uuid([253, 0, 0, n])
            .unwrap_or_else(|| panic!("{kind} has no target"));
        found.push((*kind, target));
    }
    for (kind, target) in found {
        // `Entity.entityTags()`: the set, in the order it was built. The
        // comparison sorts, so a different set order still passes.
        let mut tags = game.tags_of(target);
        tags.sort();
        assert_eq!(tags, ["a", "b", "c"], "{kind}");
        let mut saved = saved_tags(&mut game, target);
        saved.sort();
        assert_eq!(saved, ["a", "b", "c"], "{kind}");
        // `/tag <targets> list` reports the size of that set.
        let (ok, said) = run(&mut game, &format!("tag @e[type={kind},limit=1] list"));
        let said = said.expect("a reply");
        assert!(ok, "{kind}: {said}");
        assert!(said.contains("3 tag(s)"), "{kind}: {said}");
    }
}

/// `/tag remove`'s name completes from the target's own tags: `TagCommand`
/// attaches its provider to that node only, and the server resolves the
/// parsed `targets` (`getTags`) when the client asks (`ask_server`).
#[test]
#[ignore = "daily"]
fn tag_remove_completes_from_the_targets_own_tags() {
    let mut game = game();
    assert!(game.run_command(
        "summon armor_stand 4.5 100 4.5 {UUID:[I;255,0,0,1],Tags:[\"b\",\"a\",\"aa\",\"foo_bar\",\"a\"]}"
    ));
    let d = horizon_client::command::vanilla::session_dispatcher(
        &game.level.rule_registry,
        &game.level.enabled_features,
    );
    let remove = "tag @e[type=minecraft:armor_stand,limit=1] remove ";
    let parse = d.parse(remove, &game.ui.registries);
    assert_eq!(
        d.suggests_at(&parse, remove.len(), 2),
        Some(&horizon_client::command::Suggests::EntityTags)
    );
    // `TagCommand.getTags`: the target's own tags, each once.
    let mut values = game.tag_suggestion_values(&parse.context);
    values.sort();
    assert_eq!(values, ["a", "aa", "b", "foo_bar"]);
    // `SharedSuggestionProvider.suggest` keeps the tags `matchesSubStr` matches.
    let at = |command: &str| {
        d.parse(command, &game.ui.registries)
            .context
            .suggestion_context(command.len())
            .1
    };
    let texts = |command: &str| {
        let mut texts: Vec<String> = crate::game::chat::tag_suggestions(
            command,
            at(command),
            &[
                "b".to_owned(),
                "a".to_owned(),
                "aa".to_owned(),
                "foo_bar".to_owned(),
            ],
        )
        .into_iter()
        .map(|s| s.text)
        .collect();
        texts.sort();
        texts
    };
    assert_eq!(texts(remove), ["a", "aa", "b", "foo_bar"]);
    // The server's answer covers the word being completed, not (0, 0).
    let list =
        crate::game::chat::tag_suggestions(remove, at(remove), &["b".to_owned(), "a".to_owned()]);
    assert!(list.iter().all(|s| s.range == (remove.len(), remove.len())));
    // A completion past the packet's 32767 UTF-16 units is not sent; one at
    // the limit still is.
    let longest = format!("{}a", "\u{1f600}".repeat(16383));
    let too_long = "\u{1f600}".repeat(16384);
    let list = crate::game::chat::tag_suggestions(remove, at(remove), &[longest.clone(), too_long]);
    assert_eq!(
        list.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
        [longest.as_str()]
    );
    // The typed word filters, and an exact match is not suggested again.
    assert_eq!(
        texts("tag @e[type=minecraft:armor_stand,limit=1] remove a"),
        ["aa"]
    );
    // `matchesSubStr` also matches a segment after `_`, `.` or `/`.
    assert_eq!(
        texts("tag @e[type=minecraft:armor_stand,limit=1] remove bar"),
        ["foo_bar"]
    );
    // TagCommand attaches the provider to `remove` alone.
    let add = "tag @e[type=minecraft:armor_stand,limit=1] add ";
    let parse = d.parse(add, &game.ui.registries);
    assert_eq!(d.suggests_at(&parse, add.len(), 2), None);
}

/// `/data merge` loads the compound into the entity (`Entity.load`), so the
/// `Tags` it carries are a set too.
#[test]
#[ignore = "daily"]
fn data_merge_writes_the_tag_set_of_an_armour_stand() {
    let mut game = game();
    assert!(game.run_command("summon armor_stand 4.5 100 4.5 {UUID:[I;254,0,0,1]}"));
    let target = game.target_with_uuid([254, 0, 0, 1]).unwrap();
    let Target::Stand(id) = target else {
        panic!("an armour stand target");
    };
    let uuid = horizon_client::save::uuid_text(game.uuid_for(id));
    let (ok, said) = run(
        &mut game,
        &format!("data merge entity {uuid} {{Tags:[\"b\",\"a\",\"a\"]}}"),
    );
    assert!(ok, "{said:?}");
    let mut tags = game.tags_of(target);
    tags.sort();
    assert_eq!(tags, ["a", "b"]);
    let mut saved = saved_tags(&mut game, target);
    saved.sort();
    assert_eq!(saved, ["a", "b"]);
}
