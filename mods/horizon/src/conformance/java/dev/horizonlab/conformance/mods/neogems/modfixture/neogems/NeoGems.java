package modfixture.neogems;

import net.minecraft.core.registries.Registries;
import net.minecraft.world.item.Item;
import net.minecraft.world.item.Rarity;
import net.minecraft.world.level.block.Block;
import net.minecraft.world.level.block.state.BlockBehaviour;
import net.neoforged.bus.api.IEventBus;
import net.neoforged.fml.common.Mod;
import net.neoforged.neoforge.registries.DeferredBlock;
import net.neoforged.neoforge.registries.DeferredHolder;
import net.neoforged.neoforge.registries.DeferredItem;
import net.neoforged.neoforge.registries.DeferredRegister;

/** NeoForge's DeferredRegister: suppliers run when the registry event fires, here once the
 * registers are attached to the mod bus. */
@Mod(NeoGems.MOD_ID)
public final class NeoGems {
    public static final String MOD_ID = "neogems";
    public static final DeferredRegister.Items ITEMS = DeferredRegister.createItems(MOD_ID);
    public static final DeferredRegister.Blocks BLOCKS = DeferredRegister.createBlocks(MOD_ID);
    public static final DeferredRegister<Block> RAW_BLOCKS = DeferredRegister.create(Registries.BLOCK, MOD_ID);

    public static final DeferredItem<Item> AMBER = ITEMS.registerSimpleItem("amber", new Item.Properties().stacksTo(16));
    public static final DeferredHolder<Item, Item> JADE = ITEMS.register("jade",
            () -> new Item(new Item.Properties().rarity(Rarity.RARE)));
    public static final DeferredBlock<Block> AMBER_BLOCK = BLOCKS.registerSimpleBlock("amber_block",
            BlockBehaviour.Properties.of().strength(1.0F, 3.0F));
    public static final DeferredHolder<Block, Block> JADE_BLOCK = RAW_BLOCKS.register("jade_block",
            () -> new Block(BlockBehaviour.Properties.of().strength(4.0F).requiresCorrectToolForDrops()));

    public NeoGems(IEventBus modBus) {
        ITEMS.register(modBus);
        BLOCKS.register(modBus);
        RAW_BLOCKS.register(modBus);
    }
}
