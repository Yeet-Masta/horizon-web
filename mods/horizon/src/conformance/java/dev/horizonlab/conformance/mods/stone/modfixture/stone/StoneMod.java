package modfixture.stone;

import java.util.ArrayList;
import java.util.Comparator;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import net.fabricmc.api.ModInitializer;
import net.minecraft.core.Registry;
import net.minecraft.core.registries.BuiltInRegistries;
import net.minecraft.resources.Identifier;
import net.minecraft.world.item.BlockItem;
import net.minecraft.world.item.Item;
import net.minecraft.world.level.block.Block;
import net.minecraft.world.level.block.state.BlockBehaviour;

/** Blocks and their items from a table: registration order is a HashMap's iteration order, the
 * properties come from records filtered and computed with streams, the ids from String.format. */
public final class StoneMod implements ModInitializer {
    record Material(String name, float hardness, float resistance, boolean tool) {
        float scaled(float factor) {
            return hardness * factor;
        }
    }

    static final Map<String, Material> TABLE = new HashMap<>();
    static final List<String> REGISTERED = new ArrayList<>();

    static {
        for (Material m : List.of(new Material("marble", 1.5F, 6.0F, true), new Material("slate", 3.0F, 6.0F, true),
                new Material("chalk", 0.5F, 0.5F, false), new Material("basalt_brick", 2.0F, 8.0F, true),
                new Material("old_cobble", 2.5F, 6.0F, true))) {
            TABLE.put(m.name(), m);
        }
    }

    @Override
    public void onInitialize() {
        for (Map.Entry<String, Material> entry : TABLE.entrySet()) {
            Material m = entry.getValue();
            BlockBehaviour.Properties props = BlockBehaviour.Properties.of().strength(m.hardness(), m.resistance());
            if (m.tool()) {
                props = props.requiresCorrectToolForDrops();
            }
            String id = String.format("%s_%s", "polished", entry.getKey());
            Block block = Registry.register(BuiltInRegistries.BLOCK, Identifier.fromNamespaceAndPath("stone", id),
                    new Block(props));
            Registry.register(BuiltInRegistries.ITEM, Identifier.fromNamespaceAndPath("stone", id),
                    new BlockItem(block, new Item.Properties()));
            REGISTERED.add(id);
        }
        Material hardest = TABLE.values().stream().max(Comparator.comparingDouble(Material::hardness)).orElseThrow();
        Registry.register(BuiltInRegistries.BLOCK, Identifier.fromNamespaceAndPath("stone", "reinforced_" + hardest.name()),
                new Block(BlockBehaviour.Properties.of().strength(hardest.scaled(4F), 1200F).requiresCorrectToolForDrops()));
    }
}
