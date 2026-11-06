package net.minecraft.core;

import net.minecraft.resources.Identifier;

public interface Registry<T> {
    static <V, T extends V> T register(Registry<V> registry, Identifier id, T value) {
        return value;
    }

    static <V, T extends V> T register(Registry<V> registry, String id, T value) {
        return value;
    }
}
