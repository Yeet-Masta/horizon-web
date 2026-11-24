// game-module-visibility: pub(crate)
//! The executable's side of `crafting_recipe_choice`, which the client library's crafting matcher
//! asks (`native_client_hooks`) when a grid matches more than one recipe: the grid and the
//! results cross to the mods, with the loop context of the side that matches, and no `Host`.
use super::api_values::TryIntoApi;
use horizon_client::crafting::Crafter;
use horizon_client::items::ItemStack;
use horizon_world::native_load_context::LoopContext;

/// Whether a mod listens: only then does the matcher try every recipe.
pub(crate) fn listens() -> bool {
    mod_api::__private::listened(mod_api::HookId::CraftingRecipeChoice)
}

/// Ask the mods which of `recipes` the grid makes. A stack that can't cross to the mods leaves
/// `choice` at 0, the first match, and says so once per item.
pub(crate) fn choose(
    context: &LoopContext,
    crafter: Crafter,
    grid: &[Option<ItemStack>],
    width: usize,
    recipes: &[String],
    results: &[ItemStack],
    choice: &mut u32,
) {
    let Ok(width) = u32::try_from(width) else {
        return;
    };
    let cells: Result<Vec<Option<mod_api::ItemStack>>, _> = grid
        .iter()
        .map(|cell| cell.as_ref().map(TryIntoApi::try_into_api).transpose())
        .collect();
    let outputs: Result<Vec<mod_api::ItemStack>, _> =
        results.iter().map(TryIntoApi::try_into_api).collect();
    match (cells, outputs) {
        (Ok(cells), Ok(outputs)) => {
            use horizon_client::crafting::mod_kinds as kinds;
            kinds::native_crafting_recipe_choice(kinds::CraftingRecipeChoiceCall {
                context,
                values: mod_api::__private::CraftingRecipeChoiceCall {
                    player: crafter.player,
                    crafter: crafter.block,
                    grid: &cells,
                    width,
                    recipes,
                    results: &outputs,
                    choice,
                },
            });
        }
        (Err(error), _) | (_, Err(error)) => refuse(error),
    }
}

fn refuse(error: super::api_values::StackReadError) {
    static TOLD: std::sync::Once = std::sync::Once::new();
    TOLD.call_once(|| {
        log::warn!(
            "crafting_recipe_choice: a stack in the grid or a result could not cross to the mods \
             ({error:?}); the first matching recipe is made"
        );
    });
}
