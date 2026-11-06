package net.fabricmc.fabric.api.event.lifecycle.v1;

import net.fabricmc.fabric.api.event.Event;
import net.minecraft.server.MinecraftServer;

public final class ServerLifecycleEvents {
    public static final Event<ServerStarting> SERVER_STARTING = null;
    public static final Event<ServerStarted> SERVER_STARTED = null;

    @FunctionalInterface
    public interface ServerStarting {
        void onServerStarting(MinecraftServer server);
    }

    @FunctionalInterface
    public interface ServerStarted {
        void onServerStarted(MinecraftServer server);
    }
}
