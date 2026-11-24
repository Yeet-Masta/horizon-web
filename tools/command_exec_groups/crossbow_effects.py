"""CrossbowItem charge and ProjectileWeaponItem draw/shoot."""
GROUP = 'crossbow enchantment effects'

_COST = {'base': 1, 'per_level_above_first': 0}
ENCHANTMENT = {
    'description': 'Crossbow probe', 'supported_items': 'minecraft:crossbow',
    'weight': 1, 'max_level': 2, 'min_cost': _COST, 'max_cost': _COST,
    'anvil_cost': 1, 'slots': ['hand'], 'effects': {
        'minecraft:crossbow_charge_time': {'type': 'minecraft:set', 'value': 0.125},
        'minecraft:projectile_count': [{'effect': {'type': 'minecraft:set', 'value': {
            'type': 'minecraft:lookup', 'values': [4, 1025], 'fallback': 4,
        }}}],
        'minecraft:projectile_spread': [{'effect': {'type': 'minecraft:set', 'value': 35}}],
    },
}


def cases(cmd, setup):
    charged = 'SelectedItem.components."minecraft:charged_projectiles"'
    ops = setup('/kill @e[type=arrow]', '/clear @s', '/gamemode survival @s',
                '/fill 0 229 0 15 229 15 stone', '/tp @s 8.5 230 8.5 0 0')
    for name, enchants, ticks, shots in [
        ('multishot II', '{"minecraft:quick_charge":4,"minecraft:multishot":2}', 6, 5),
        ('world-pack volley', '{"crossbow_test:volley":1}', 3, 4),
    ]:
        ops += setup('/clear @s',
                     '/item replace entity @s weapon.mainhand with minecraft:crossbow'
                     f'[enchantments={enchants}]', '/give @s minecraft:arrow 8')
        ops += [
            {'op': 'use_item'},
            {'op': 'player_loop', 'label': name, 'n': ticks},
            cmd(f'/execute if data entity @s {charged}[{shots - 1}]', expect_result=1),
            cmd('/clear @s minecraft:arrow 0', expect_result=7),
            {'op': 'release_item'},
            {'op': 'use_item'},
            cmd('/execute if entity @e[type=arrow]', expect_result=shots),
            {'op': 'tick', 'n': 1},
            # Observe the two outer arrows after they separate. Their lateral
            # speeds exceed one block/tick at 20 and 35 degrees.
            cmd('/data get entity @e[type=arrow,x=100,sort=nearest,limit=1] Motion[0] 1',
                expect_result=1),
            cmd('/data get entity @e[type=arrow,x=-100,sort=nearest,limit=1] Motion[0] 1',
                expect_result=-2),
            *setup('/kill @e[type=arrow]'),
        ]
    ops += setup('/clear @s',
                 '/item replace entity @s weapon.mainhand with minecraft:crossbow'
                 '[enchantments={"crossbow_test:volley":2}]', '/give @s minecraft:arrow 8')
    ops += [
        {'op': 'use_item'},
        {'op': 'player_loop', 'label': 'world-pack volley at the component limit', 'n': 3},
        cmd(f'/execute if data entity @s {charged}[1023]', expect_result=1),
        cmd(f'/execute unless data entity @s {charged}[1024]', expect_result=1),
        cmd('/clear @s minecraft:arrow 0', expect_result=7),
        {'op': 'release_item'},
    ]
    return ops + setup('/clear @s', '/gamemode creative @s')
