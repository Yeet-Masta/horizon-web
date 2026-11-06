package net.minecraft.world.level.block;

import net.minecraft.world.level.block.state.BlockBehaviour;

public class Block extends BlockBehaviour {
    public Block(BlockBehaviour.Properties properties) {}

    public float getFriction() {
        return 0.6F;
    }
}
