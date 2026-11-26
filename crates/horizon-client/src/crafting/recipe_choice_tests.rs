//! `crafting_recipe_choice` in the matcher, through the library's test callbacks: they list every
//! match and pick the recipe a context's `recipe_choice` setting names.
use super::*;
use crate::native_client_hooks::test_hooks::install;
use horizon_world::native_load_context::{DataLoadOrigin, LoadContext, LoadInputs, LoopSide};
use std::collections::HashMap;

/// Two recipes take one oak log, the second one listed first-come in file order.
fn conflicting() -> Recipes {
    let named = |name: &str, json: &str| {
        let file = Arc::new(serde_json::from_str::<Value>(json).unwrap());
        crate::crafting::ResolvedRecipe {
            name: name.into(),
            resolved: file.clone(),
            file,
        }
    };
    let sources = crate::crafting::ResolvedSources {
        recipes: vec![
            named(
                "a:planks",
                r#"{"type":"minecraft:crafting_shapeless","ingredients":["minecraft:oak_log"],
                    "result":{"id":"minecraft:oak_planks","count":4}}"#,
            ),
            named(
                "b:sticks",
                r#"{"type":"minecraft:crafting_shapeless","ingredients":["minecraft:oak_log"],
                    "result":{"id":"minecraft:stick","count":8}}"#,
            ),
        ],
        ..Default::default()
    };
    let items = crate::items::Items::for_tests(HashMap::new());
    Recipes::from_resolved(
        &horizon_std::env::temp_dir(),
        &items,
        &horizon_world::loot::PackOverlay::NONE,
        &sources,
    )
    .unwrap()
}

/// A server loop context whose `recipe_choice` setting `pick` names `recipe`.
fn picking(recipe: &str) -> LoopContext {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let settings = horizon_std::env::temp_dir().join(format!(
        "horizon-recipe-choice-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let file = settings.join("recipe_choice").join("pick");
    horizon_std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    horizon_std::fs::write(&file, recipe).unwrap();
    let inputs = Arc::new(LoadInputs::new(Default::default(), settings));
    LoadContext::new(DataLoadOrigin::ServerDataLoad, inputs, [])
        .loop_context(LoopSide::Server, ["recipe_choice"])
}

fn made(
    recipes: &Recipes,
    context: &LoopContext,
    grid: &[Option<ItemStack>],
    size: usize,
) -> Option<(String, String, u32)> {
    recipes
        .find(context, grid, size)
        .map(|(id, out)| (id.to_owned(), out.item.to_string(), out.count))
}

#[test]
fn the_first_match_is_made_until_a_mod_picks_another() {
    install();
    let recipes = conflicting();
    let log = Some(ItemStack::new("minecraft:oak_log", 1));
    let grid = [log.clone(), None, None, None];
    let vanilla = crate::test_resources::server_loop_context();
    let first = made(&recipes, vanilla, &grid, 2).unwrap();
    assert_eq!(first.1, "minecraft:oak_planks");
    let sticks = made(&recipes, &picking("b:sticks"), &grid, 2).unwrap();
    assert_eq!(
        sticks,
        ("b:sticks".to_owned(), "minecraft:stick".to_owned(), 8)
    );
    // In a crafting table's grid too, wherever the log lies.
    let mut table = vec![None; 9];
    table[4] = log;
    let sticks = made(&recipes, &picking("b:sticks"), &table, 3).unwrap();
    assert_eq!(sticks.0, "b:sticks");
    // A recipe that doesn't match leaves the first.
    let other = made(&recipes, &picking("c:none"), &grid, 2).unwrap();
    assert_eq!(other.1, "minecraft:oak_planks");
}

#[test]
fn one_match_and_none_are_as_before() {
    install();
    let recipes = conflicting();
    let dirt = [Some(ItemStack::new("minecraft:dirt", 1)), None, None, None];
    assert_eq!(made(&recipes, &picking("b:sticks"), &dirt, 2), None);
    let empty = [None, None, None, None];
    assert_eq!(made(&recipes, &picking("b:sticks"), &empty, 2), None);
}

#[test]
fn the_choice_is_told_who_crafts() {
    install();
    let recipes = conflicting();
    let grid = [
        Some(ItemStack::new("minecraft:oak_log", 1)),
        None,
        None,
        None,
    ];
    let told = |crafter: Crafter| {
        crate::native_client_hooks::test_hooks::TOLD_CRAFTER.with(|t| t.set(None));
        recipes.find_for(&picking("b:sticks"), crafter, &grid, 2, &|_| None);
        crate::native_client_hooks::test_hooks::TOLD_CRAFTER.with(std::cell::Cell::get)
    };
    let player = Crafter::player(Some(0x1234));
    assert_eq!(told(player), Some(player));
    let block = Crafter::block([4, 64, -9]);
    assert_eq!(told(block), Some(block));
    // A lookup on nobody's behalf says so.
    assert_eq!(
        crate::native_client_hooks::test_hooks::TOLD_CRAFTER.with(|t| {
            t.set(None);
            recipes.find(&picking("b:sticks"), &grid, 2);
            t.get()
        }),
        Some(Crafter::NOBODY)
    );
}

#[test]
fn with_no_listener_the_first_match_is_made_and_nobody_is_asked() {
    use crate::native_client_hooks::test_hooks::{NOT_LISTENING, TOLD_CRAFTER};
    install();
    let recipes = conflicting();
    let grid = [
        Some(ItemStack::new("minecraft:oak_log", 1)),
        None,
        None,
        None,
    ];
    NOT_LISTENING.with(|n| n.set(true));
    TOLD_CRAFTER.with(|t| t.set(None));
    let made = made(&recipes, &picking("b:sticks"), &grid, 2);
    let asked = TOLD_CRAFTER.with(std::cell::Cell::get);
    NOT_LISTENING.with(|n| n.set(false));
    assert_eq!(made.unwrap().1, "minecraft:oak_planks");
    assert_eq!(asked, None);
}
