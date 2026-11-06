package modfixture.gems;

import net.fabricmc.api.ModInitializer;

/** Items held in static fields of a holder class, registered from an enum in a static block. */
public final class GemsMod implements ModInitializer {
    public static final String MOD_ID = "gems";

    @Override
    public void onInitialize() {
        GemItems.init();
    }
}
