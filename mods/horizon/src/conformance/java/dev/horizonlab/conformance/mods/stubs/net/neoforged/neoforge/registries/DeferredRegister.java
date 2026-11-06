package net.neoforged.neoforge.registries;

import java.util.Collection;
import java.util.function.Function;
import java.util.function.Supplier;
import net.minecraft.core.Registry;
import net.minecraft.world.item.Item;
import net.minecraft.world.level.block.Block;
import net.minecraft.world.level.block.state.BlockBehaviour;
import net.neoforged.bus.api.IEventBus;

public class DeferredRegister<T> {
    public static <T> DeferredRegister<T> create(Registry<T> registry, String namespace) {
        return null;
    }

    public static Items createItems(String namespace) {
        return null;
    }

    public static Blocks createBlocks(String namespace) {
        return null;
    }

    public <I extends T> DeferredHolder<T, I> register(String name, Supplier<? extends I> supplier) {
        return null;
    }

    public void register(IEventBus bus) {}

    public Collection<DeferredHolder<T, ? extends T>> getEntries() {
        return null;
    }

    public static class Items extends DeferredRegister<Item> {
        public DeferredItem<Item> registerSimpleItem(String name, Item.Properties properties) {
            return null;
        }

        public <I extends Item> DeferredItem<I> registerItem(String name, Function<Item.Properties, ? extends I> factory,
                Item.Properties properties) {
            return null;
        }
    }

    public static class Blocks extends DeferredRegister<Block> {
        public DeferredBlock<Block> registerSimpleBlock(String name, BlockBehaviour.Properties properties) {
            return null;
        }
    }
}
