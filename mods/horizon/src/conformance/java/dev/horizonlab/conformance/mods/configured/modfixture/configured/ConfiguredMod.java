package modfixture.configured;

import java.io.IOException;
import java.io.InputStream;
import java.util.Properties;
import java.util.TreeMap;
import net.fabricmc.api.ModInitializer;
import net.minecraft.core.Registry;
import net.minecraft.core.registries.BuiltInRegistries;
import net.minecraft.resources.Identifier;
import net.minecraft.world.item.Item;

/** Reads a properties file bundled in its own jar and registers one item per entry, in key order,
 * with the stack size the file names. */
public final class ConfiguredMod implements ModInitializer {
    @Override
    public void onInitialize() {
        Properties config = new Properties();
        try (InputStream in = ConfiguredMod.class.getResourceAsStream("/configured.properties")) {
            if (in == null) {
                throw new IllegalStateException("configured.properties missing");
            }
            config.load(in);
        } catch (IOException e) {
            throw new IllegalStateException(e);
        }
        for (var entry : new TreeMap<>(config).entrySet()) {
            String name = entry.getKey().toString();
            int stack = Integer.parseInt(entry.getValue().toString().trim());
            Registry.register(BuiltInRegistries.ITEM, Identifier.fromNamespaceAndPath("configured", name),
                    new Item(new Item.Properties().stacksTo(Math.max(1, Math.min(stack, 99)))));
        }
    }
}
