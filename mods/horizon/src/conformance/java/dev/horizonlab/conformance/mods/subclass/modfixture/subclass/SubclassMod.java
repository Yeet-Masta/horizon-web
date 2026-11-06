package modfixture.subclass;

import net.fabricmc.api.ModInitializer;
import net.minecraft.core.Registry;
import net.minecraft.core.registries.BuiltInRegistries;
import net.minecraft.resources.Identifier;
import net.minecraft.world.item.Item;
import net.minecraft.world.item.Rarity;
import net.minecraft.world.level.block.Block;
import net.minecraft.world.level.block.state.BlockBehaviour;

/** The mod's own subclasses of host classes, with fields of their own and overrides: an Item and
 * a Block. */
public final class SubclassMod implements ModInitializer {
    static final class ChargedItem extends Item {
        private final int maxCharge;
        private int charge;

        ChargedItem(Item.Properties properties, int maxCharge) {
            super(properties.stacksTo(1).rarity(Rarity.EPIC));
            this.maxCharge = maxCharge;
        }

        int charge(int amount) {
            charge = Math.min(maxCharge, charge + amount);
            return charge;
        }

        @Override
        public String getDescriptionId() {
            return "item.subclass.charged." + maxCharge;
        }
    }

    static class IceBlock extends Block {
        private final float slip;

        IceBlock(BlockBehaviour.Properties properties, float slip) {
            super(properties.friction(slip));
            this.slip = slip;
        }

        @Override
        public float getFriction() {
            return slip;
        }
    }

    @Override
    public void onInitialize() {
        ChargedItem wand = new ChargedItem(new Item.Properties(), 100);
        wand.charge(60);
        int charged = wand.charge(60);
        Registry.register(BuiltInRegistries.ITEM, Identifier.fromNamespaceAndPath("subclass", "wand_" + charged), wand);
        IceBlock ice = new IceBlock(BlockBehaviour.Properties.of().strength(0.5F), 0.98F);
        String id = ice.getFriction() > 0.9F ? "slippery_ice" : "ice";
        Registry.register(BuiltInRegistries.BLOCK, Identifier.fromNamespaceAndPath("subclass", id), ice);
        Registry.register(BuiltInRegistries.ITEM, Identifier.fromNamespaceAndPath("subclass", wand.getDescriptionId()
                .substring("item.subclass.".length()).replace('.', '_')), new Item(new Item.Properties().stacksTo(3)));
    }
}
