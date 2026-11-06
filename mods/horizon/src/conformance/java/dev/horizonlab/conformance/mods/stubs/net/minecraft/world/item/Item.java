package net.minecraft.world.item;

public class Item {
    public Item(Properties properties) {}

    public String getDescriptionId() {
        return null;
    }

    public static class Properties {
        public Properties() {}

        public Properties stacksTo(int count) {
            return this;
        }

        public Properties durability(int maxDamage) {
            return this;
        }

        public Properties rarity(Rarity rarity) {
            return this;
        }

        public Properties fireResistant() {
            return this;
        }
    }
}
