package net.minecraft.world.level.block.state;

public abstract class BlockBehaviour {
    public static class Properties {
        public Properties() {}

        public static Properties of() {
            return new Properties();
        }

        public Properties strength(float destroyTime) {
            return this;
        }

        public Properties strength(float destroyTime, float explosionResistance) {
            return this;
        }

        public Properties friction(float friction) {
            return this;
        }

        public Properties requiresCorrectToolForDrops() {
            return this;
        }

        public Properties noOcclusion() {
            return this;
        }

        public Properties instabreak() {
            return this;
        }
    }
}
