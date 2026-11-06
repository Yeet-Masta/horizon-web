package modfixture.gems;

import java.util.EnumMap;
import java.util.Locale;
import java.util.Map;
import net.minecraft.core.Registry;
import net.minecraft.core.registries.BuiltInRegistries;
import net.minecraft.resources.Identifier;
import net.minecraft.world.item.Item;
import net.minecraft.world.item.Rarity;

public final class GemItems {
    enum Gem {
        RUBY(16, Rarity.UNCOMMON),
        SAPPHIRE(32, Rarity.RARE),
        TOPAZ(64, Rarity.COMMON);

        final int stack;
        final Rarity rarity;

        Gem(int stack, Rarity rarity) {
            this.stack = stack;
            this.rarity = rarity;
        }
    }

    public static final Map<Gem, Item> GEMS = new EnumMap<>(Gem.class);
    public static final Item GEM_DUST = register("gem_dust", new Item.Properties().stacksTo(8));

    static {
        for (Gem gem : Gem.values()) {
            GEMS.put(gem, register(gem.name().toLowerCase(Locale.ROOT), new Item.Properties().stacksTo(gem.stack)
                    .rarity(gem.rarity)));
        }
    }

    public static final Item POLISHED_RUBY = register("polished_" + Gem.RUBY.name().toLowerCase(Locale.ROOT),
            new Item.Properties().stacksTo(Gem.RUBY.stack / 2).fireResistant());

    static Item register(String name, Item.Properties properties) {
        return Registry.register(BuiltInRegistries.ITEM, Identifier.fromNamespaceAndPath(GemsMod.MOD_ID, name),
                new Item(properties));
    }

    static void init() {}
}
