package net.neoforged.neoforge.registries;

import java.util.function.Supplier;

public class DeferredHolder<R, T extends R> implements Supplier<T> {
    @Override
    public T get() {
        return null;
    }
}
