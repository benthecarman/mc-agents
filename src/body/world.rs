//! Reading the world: blocks, entities and time of day.

use std::collections::HashMap;

use azalea::{
    block::{BlockState, BlockStates},
    ecs::{entity::Entity, query::Without},
    entity::{Dead, EntityKindComponent, EntityUuid, Position},
    registry::builtin::BlockKind,
    BlockPos, Client, Vec3,
};
use serde_json::{json, Value};

use super::strip_ns;

/// `day_time` from the server, in ticks. A day is 24000 ticks.
#[derive(Debug, Clone, Copy)]
pub struct TimeOfDay(pub u64);

impl TimeOfDay {
    pub fn ticks(self) -> u64 {
        self.0 % 24000
    }

    pub fn phase(self) -> &'static str {
        match self.ticks() {
            0..=999 => "morning",
            1000..=10999 => "day",
            11000..=12999 => "dusk",
            13000..=22999 => "night",
            _ => "dawn",
        }
    }

    pub fn is_dark(self) -> bool {
        (12500..23500).contains(&self.ticks())
    }

    /// Vanilla lets players sleep between these ticks.
    pub fn can_sleep(self) -> bool {
        (12542..=23459).contains(&self.ticks())
    }

    /// Real seconds until dark at 20 ticks per second (0 when already dark).
    pub fn ticks_until_dark(self) -> u64 {
        if self.is_dark() {
            0
        } else if self.ticks() < 12500 {
            12500 - self.ticks()
        } else {
            24000 - self.ticks() + 12500
        }
    }

    pub fn day(self) -> u64 {
        self.0 / 24000
    }

    pub fn describe(self) -> String {
        format!("{} (tick {})", self.phase(), self.ticks())
    }
}

#[derive(Debug, Clone)]
pub struct EntityInfo {
    pub entity: Entity,
    pub kind: String,
    pub name: Option<String>,
    pub pos: Vec3,
    pub distance: f64,
    pub hostile: bool,
}

impl EntityInfo {
    pub fn short(&self) -> String {
        match &self.name {
            Some(name) => format!("player {name} {:.0}m", self.distance),
            None => format!("{}{} {:.0}m", self.kind, if self.hostile { "!" } else { "" }, self.distance),
        }
    }

    pub fn json(&self) -> Value {
        json!({
            "type": self.kind,
            "name": self.name,
            "at": [self.pos.x.floor(), self.pos.y.floor(), self.pos.z.floor()],
            "distance": self.distance.round(),
            "hostile": self.hostile,
        })
    }
}

const HOSTILE: [&str; 22] = [
    "zombie", "husk", "drowned", "zombie_villager", "skeleton", "stray", "bogged", "creeper",
    "spider", "cave_spider", "witch", "slime", "phantom", "pillager", "vindicator", "evoker",
    "silverfish", "enderman", "blaze", "breeze", "warden", "zombified_piglin",
];

pub fn entities(client: &Client, radius: f64) -> Vec<EntityInfo> {
    let me = client.position();
    let my_uuid = client.uuid();
    let tab = client.tab_list();
    let mut ecs = client.ecs.lock();
    let mut query =
        ecs.query_filtered::<(Entity, &Position, &EntityKindComponent, Option<&EntityUuid>), Without<Dead>>();
    let mut out: Vec<EntityInfo> = Vec::new();
    for (entity, pos, kind, uuid) in query.iter(&ecs) {
        if entity == client.entity || uuid.is_some_and(|u| **u == my_uuid) {
            continue;
        }
        let pos = **pos;
        let distance = me.distance_to(pos);
        if distance > radius || distance < 0.05 {
            continue;
        }
        let kind = strip_ns(&kind.0.to_string());
        let name = if kind == "player" {
            uuid.and_then(|u| tab.get(&**u)).map(|p| p.profile.name.clone())
        } else {
            None
        };
        if kind == "player" && out.iter().any(|e| e.name == name) {
            continue; // the same player seen by several of our bots
        }
        let hostile = HOSTILE.contains(&kind.as_str());
        out.push(EntityInfo {
            entity,
            kind,
            name,
            pos,
            distance,
            hostile,
        });
    }
    out.sort_by(|a, b| a.distance.total_cmp(&b.distance));
    out
}

fn all_block_kinds() -> impl Iterator<Item = BlockKind> {
    (0u32..).map_while(|i| <BlockKind as azalea::registry::Registry>::from_u32(i))
}

/// Turns a name such as `oak_log`, `log`, `iron_ore` or `bed` into the block
/// kinds it means.
pub fn resolve_blocks(query: &str) -> Result<Vec<BlockKind>, String> {
    let q = strip_ns(query).to_lowercase();
    let kinds: Vec<BlockKind> = all_block_kinds()
        .filter(|k| {
            let name = strip_ns(&k.to_string());
            match q.as_str() {
                "log" | "logs" | "wood" | "tree" => name.ends_with("_log") && !name.starts_with("stripped"),
                "bed" | "beds" => name.ends_with("_bed"),
                "planks" => name.ends_with("_planks"),
                "leaves" => name.ends_with("_leaves"),
                "wool" => name.ends_with("_wool"),
                "stone" => name == "stone" || name == "cobblestone",
                "ore" | "ores" => name.ends_with("_ore"),
                _ => name == q || name == format!("deepslate_{q}"),
            }
        })
        .collect();
    if kinds.is_empty() {
        Err(format!("unknown block '{query}'"))
    } else {
        Ok(kinds)
    }
}

pub fn nearest_block(client: &Client, kinds: &[BlockKind], max_distance: i32, skip: &[BlockPos]) -> Option<BlockPos> {
    let me = BlockPos::from(client.position());
    let states = BlockStates::from(kinds);
    let world = client.world();
    let world = world.read();
    world
        .find_blocks(me, &states)
        .take(256)
        .filter(|p| !skip.contains(p))
        .filter(|p| (p.x - me.x).abs() <= max_distance && (p.z - me.z).abs() <= max_distance && (p.y - me.y).abs() <= max_distance)
        .min_by_key(|p| {
            let (dx, dy, dz) = (p.x - me.x, p.y - me.y, p.z - me.z);
            dx * dx + dy * dy * 2 + dz * dz
        })
}

pub fn block_state(client: &Client, pos: BlockPos) -> Option<BlockState> {
    client.world().read().get_block_state(pos)
}

pub fn block_name(client: &Client, pos: BlockPos) -> Option<String> {
    block_state(client, pos).map(|s| strip_ns(&BlockKind::from(s).to_string()))
}

const REPLACEABLE: [&str; 9] = [
    "air", "cave_air", "void_air", "water", "short_grass", "tall_grass", "fern", "snow", "dead_bush",
];

pub fn is_replaceable(client: &Client, pos: BlockPos) -> bool {
    block_name(client, pos).is_some_and(|n| REPLACEABLE.contains(&n.as_str()))
}

fn is_solid(client: &Client, pos: BlockPos) -> bool {
    block_name(client, pos).is_some_and(|n| !REPLACEABLE.contains(&n.as_str()) && n != "lava")
}

/// A solid neighbor of `target` to click, and the point on its face to look at.
pub fn support_face(client: &Client, target: BlockPos) -> Option<(BlockPos, Vec3)> {
    let offsets = [(0, -1, 0), (1, 0, 0), (-1, 0, 0), (0, 0, 1), (0, 0, -1), (0, 1, 0)];
    offsets.into_iter().find_map(|(dx, dy, dz)| {
        let neighbor = BlockPos::new(target.x + dx, target.y + dy, target.z + dz);
        is_solid(client, neighbor).then(|| {
            let face = neighbor.center() - Vec3::new(dx as f64 * 0.5, dy as f64 * 0.5, dz as f64 * 0.5);
            (neighbor, face)
        })
    })
}

const BORING: [&str; 16] = [
    "air", "cave_air", "void_air", "stone", "dirt", "grass_block", "deepslate", "granite",
    "diorite", "andesite", "tuff", "gravel", "bedrock", "short_grass", "tall_grass", "netherrack",
];

fn notable(name: &str) -> bool {
    name.ends_with("_log")
        || name.ends_with("_ore")
        || name.ends_with("_bed")
        || name.contains("chest")
        || matches!(
            name,
            "crafting_table" | "furnace" | "water" | "lava" | "wheat" | "sugar_cane" | "pumpkin" | "melon"
        )
}

/// Counts the blocks around a position and notes where the nearest of each is.
pub fn scan_blocks(client: &Client, center: BlockPos, radius: i32) -> Vec<Value> {
    let world = client.world();
    let world = world.read();
    let mut found: HashMap<String, (u32, BlockPos, i32)> = HashMap::new();
    let vertical = (radius / 2).max(6);
    for dx in -radius..=radius {
        for dz in -radius..=radius {
            for dy in -vertical..=vertical {
                let pos = BlockPos::new(center.x + dx, center.y + dy, center.z + dz);
                let Some(state) = world.get_block_state(pos) else { continue };
                if state.is_air() {
                    continue;
                }
                let name = strip_ns(&BlockKind::from(state).to_string());
                let d = dx * dx + dy * dy + dz * dz;
                let entry = found.entry(name).or_insert((0, pos, i32::MAX));
                entry.0 += 1;
                if d < entry.2 {
                    entry.1 = pos;
                    entry.2 = d;
                }
            }
        }
    }
    let mut list: Vec<(String, (u32, BlockPos, i32))> =
        found.into_iter().filter(|(n, _)| !BORING.contains(&n.as_str())).collect();
    list.sort_by_key(|(n, (count, _, d))| (!notable(n), *d, std::cmp::Reverse(*count)));
    list.into_iter()
        .take(18)
        .map(|(name, (count, pos, d))| {
            json!({"block": name, "count": count, "nearest": [pos.x, pos.y, pos.z], "distance": (d as f64).sqrt().round()})
        })
        .collect()
}
