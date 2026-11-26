//! The `crafting_recipe_choice` hook: declarations the mod API generator reads (see `hooks/mod.rs`).

pub trait Hooks {
    /// The ingredients in a crafting grid match more than one crafting recipe: which one is made
    /// (`RecipeManager.getRecipeFor` takes the first; Polymorph lets the player pick). `recipes`
    /// are the matching recipes' ids (`minecraft:oak_planks`), in the order the game tries them,
    /// and `results` what each makes; `grid` is the cells the recipes were matched against, row
    /// by row, `width` wide (2 for the inventory, 3 for a crafting table). `choice` arrives as 0,
    /// the first match, vanilla's; set it to the index of the recipe to make. An index past the
    /// end keeps the first. A match that makes nothing is not listed, and a grid whose first
    /// match makes nothing makes nothing, as in vanilla. `player` is the uuid of the player whose
    /// grid it is (their inventory, a crafting table or a mod's crafting menu they use, on the
    /// server and in their client's prediction), and `crafter` the position of a crafter block
    /// crafting on its own or shown in its menu; both are `None` for a lookup made on nobody's
    /// behalf (`RecipesHost`, a sheep's dye mix), so a mod keeping a choice per player can tell.
    ///
    /// Asked wherever the game matches a crafting grid, on both sides: the result slot of the
    /// inventory, a crafting table and a mod's crafting menu, the take and its remainders (so
    /// `item_crafted` names the chosen recipe), the crafter, and `RecipesHost` lookups. A grid
    /// is matched several times for one craft, so answer from the arguments and state the mod
    /// keeps for itself, the same way each time: the client's prediction, the server and the
    /// take's re-check must agree. It is given the loop `context` of the side that matches and
    /// takes no `Host`, so answer fast. With no listener the game stops at the first match as
    /// before; with one, every crafting recipe is tried for a grid. Beyond NeoForge: there is
    /// no counterpart. Diverges from upstream: new hook.
    #[hook(
        side = "both",
        contract = "listen",
        context = "loop",
        cancellable = false,
        target = "horizon_client_lib::crafting::mod_kinds::native_crafting_recipe_choice",
        neoforge = "none (Polymorph replaces RecipeManager.getRecipeFor with a mixin)"
    )]
    fn crafting_recipe_choice(
        context: &LoopContext,
        player: Option<u128>,
        crafter: Option<[i32; 3]>,
        grid: &[Option<ItemStack>],
        width: u32,
        recipes: &[String],
        results: &[ItemStack],
        choice: &mut u32,
    );
}
