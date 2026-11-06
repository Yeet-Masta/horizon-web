package modfixture.events;

import java.util.LinkedHashMap;
import java.util.Map;
import java.util.function.Function;
import java.util.function.Supplier;
import net.fabricmc.api.ModInitializer;
import net.fabricmc.fabric.api.event.lifecycle.v1.ServerLifecycleEvents;
import net.minecraft.core.Registry;
import net.minecraft.core.registries.BuiltInRegistries;
import net.minecraft.resources.Identifier;
import net.minecraft.world.item.Item;
import net.minecraft.world.item.Rarity;

/** Registration through lambdas: suppliers kept in an ordered map and called later, a factory
 * function applied per entry, and an event listener registered as a lambda. */
public final class EventsMod implements ModInitializer {
    static int started;

    @Override
    public void onInitialize() {
        Map<String, Supplier<Item>> pending = new LinkedHashMap<>();
        Function<Integer, Supplier<Item>> stackOf = n -> () -> new Item(new Item.Properties().stacksTo(n));
        pending.put("pebble", stackOf.apply(64));
        pending.put("crystal", () -> new Item(new Item.Properties().stacksTo(16).rarity(Rarity.RARE)));
        pending.put("orb", stackOf.andThen(s -> (Supplier<Item>) () -> new Item(new Item.Properties().stacksTo(1)
                .rarity(Rarity.EPIC))).apply(0));
        pending.forEach((name, supplier) -> Registry.register(BuiltInRegistries.ITEM,
                Identifier.fromNamespaceAndPath("events", name), supplier.get()));
        ServerLifecycleEvents.SERVER_STARTED.register(server -> started++);
    }
}
