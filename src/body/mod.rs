//! The bot body: everything it can sense and do in Minecraft, on top of azalea.
//!
//! Every action returns short text: JSON on success, or an error sentence that
//! says what went wrong and, where possible, what to do instead. The agent
//! reads these strings, so they are written for a model, not a human.

pub mod recipes;
mod world;

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use azalea::{
    entity::{inventory::Inventory, Pose},
    pathfinder::goals::RadiusGoal,
    prelude::*,
    BlockPos, Client, Vec3,
};
use azalea_inventory::{
    operations::{ClickOperation, PickupClick, SwapClick},
    ItemStack, Menu,
};
use serde_json::json;

pub use world::{EntityInfo, TimeOfDay};

pub type ActionResult = Result<String, String>;

/// Things the body learns from packets rather than from the ECS.
#[derive(Default)]
pub struct Senses {
    pub day_time: u64,
    pub thundering: bool,
    /// System chat and action-bar lines, e.g. "You can sleep only at night".
    pub system_messages: VecDeque<(Instant, String)>,
    /// Minecraft entity id of whatever hurt us last, and when.
    pub last_attacker: Option<(u32, Instant)>,
    /// The bed we last slept in.
    pub bed: Option<BlockPos>,
}

#[derive(Clone)]
pub struct Body {
    pub name: String,
    pub client: Client,
    pub senses: Arc<Mutex<Senses>>,
}

pub fn item_name(stack: &ItemStack) -> Option<(String, i32)> {
    match stack {
        ItemStack::Present(data) => Some((strip_ns(&data.kind.to_string()), data.count)),
        ItemStack::Empty => None,
    }
}

pub fn strip_ns(name: &str) -> String {
    name.trim_start_matches("minecraft:").to_string()
}

const FOODS: [&str; 18] = [
    "cooked_beef", "cooked_porkchop", "cooked_mutton", "cooked_chicken", "cooked_salmon",
    "cooked_cod", "cooked_rabbit", "bread", "baked_potato", "golden_carrot", "apple", "carrot",
    "melon_slice", "sweet_berries", "beef", "porkchop", "mutton", "chicken",
];

impl Body {
    pub fn new(name: String, client: Client, senses: Arc<Mutex<Senses>>) -> Self {
        Self {
            name,
            client,
            senses,
        }
    }

    // ----- senses ---------------------------------------------------------

    pub fn position(&self) -> Vec3 {
        self.client.position()
    }

    pub fn block_pos(&self) -> BlockPos {
        BlockPos::from(self.client.position())
    }

    pub fn health(&self) -> f32 {
        self.client.health()
    }

    pub fn food(&self) -> u32 {
        self.client.hunger().food
    }

    pub fn time(&self) -> TimeOfDay {
        TimeOfDay(self.senses.lock().unwrap().day_time)
    }

    pub fn is_sleeping(&self) -> bool {
        self.client.get_component::<Pose>() == Some(Pose::Sleeping)
    }

    fn inventory_menu(&self) -> Menu {
        self.client
            .query_self::<&Inventory, _>(|inv| inv.inventory_menu.clone())
    }

    /// Item counts across the main inventory, hotbar and offhand.
    pub fn inventory_counts(&self) -> BTreeMap<String, i32> {
        let menu = self.inventory_menu();
        let mut counts = BTreeMap::new();
        for (i, slot) in menu.slots().iter().enumerate() {
            if i < 9 {
                continue; // crafting grid and armor
            }
            if let Some((name, n)) = item_name(slot) {
                *counts.entry(name).or_insert(0) += n;
            }
        }
        counts
    }

    pub fn has_food(&self) -> bool {
        let inv = self.inventory_counts();
        FOODS.iter().any(|f| inv.contains_key(*f))
    }

    pub fn count(&self, item: &str) -> i32 {
        self.inventory_counts().get(item).copied().unwrap_or(0)
    }

    pub fn free_slots(&self) -> usize {
        let menu = self.inventory_menu();
        menu.slots()[9..=44].iter().filter(|s| s.is_empty()).count()
    }

    pub fn entities(&self, radius: f64) -> Vec<EntityInfo> {
        world::entities(&self.client, radius)
    }

    pub fn hostiles(&self, radius: f64) -> Vec<EntityInfo> {
        self.entities(radius).into_iter().filter(|e| e.hostile).collect()
    }

    /// Recent system messages since `since`, newest last.
    pub fn system_messages_since(&self, since: Instant) -> Vec<String> {
        self.senses
            .lock()
            .unwrap()
            .system_messages
            .iter()
            .filter(|(t, _)| *t >= since)
            .map(|(_, m)| m.clone())
            .collect()
    }

    /// One-line summary of the body for the model's snapshot.
    pub fn snapshot(&self) -> String {
        let p = self.block_pos();
        let inv = self.inventory_counts();
        let inv_text = if inv.is_empty() {
            "empty".to_string()
        } else {
            inv.iter()
                .map(|(k, v)| format!("{k} x{v}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let near: Vec<String> = self
            .entities(24.0)
            .iter()
            .take(8)
            .map(|e| e.short())
            .collect();
        format!(
            "Position: {} {} {}. HP: {:.0}/20. Food: {}/20. Time: {}. Inventory: {}. Nearby: {}.",
            p.x,
            p.y,
            p.z,
            self.health(),
            self.food(),
            self.time().describe(),
            inv_text,
            if near.is_empty() { "nothing".into() } else { near.join(", ") }
        )
    }

    // ----- tools ----------------------------------------------------------

    pub fn look_around(&self, radius: i32) -> ActionResult {
        let radius = radius.clamp(4, 24);
        let p = self.block_pos();
        let blocks = world::scan_blocks(&self.client, p, radius);
        let entities: Vec<_> = self.entities(radius as f64 * 1.5).iter().map(|e| e.json()).collect();
        Ok(json!({
            "position": [p.x, p.y, p.z],
            "time": self.time().describe(),
            "hp": self.health().round(),
            "food": self.food(),
            "blocks": blocks,
            "entities": entities,
        })
        .to_string())
    }

    pub fn inventory(&self) -> ActionResult {
        let held = item_name(&self.client.get_held_item()).map(|(n, _)| n);
        Ok(json!({
            "items": self.inventory_counts(),
            "held": held,
            "free_slots": self.free_slots(),
        })
        .to_string())
    }

    pub fn say(&self, text: &str) -> ActionResult {
        let text: String = text.chars().take(240).collect();
        if text.trim().is_empty() {
            return Err("nothing to say".into());
        }
        self.client.chat(text);
        Ok("said".into())
    }

    /// Walks to within `radius` of a point. Fails with the distance left if it
    /// cannot get there.
    pub async fn walk_to(&self, target: Vec3, radius: f32, timeout: Duration) -> ActionResult {
        let start = self.position();
        let close_enough = |me: Vec3| me.distance_to(target) <= radius as f64 + 0.75;
        if close_enough(start) {
            return Ok("already there".into());
        }
        self.client.start_goto(RadiusGoal::new(target, radius));
        self.client.wait_updates(1).await;
        let deadline = Instant::now() + timeout;
        let mut last_progress = (Instant::now(), self.position());
        let mut ticks = self.client.get_tick_broadcaster();
        loop {
            let _ = ticks.recv().await;
            let me = self.position();
            if self.client.is_goto_target_reached() {
                break;
            }
            if me.distance_to(last_progress.1) > 1.0 {
                last_progress = (Instant::now(), me);
            }
            if Instant::now() > deadline || last_progress.0.elapsed() > Duration::from_secs(12) {
                self.client.stop_pathfinding();
                break;
            }
        }
        let me = self.position();
        if close_enough(me) {
            Ok(format!(
                "arrived at {:.0} {:.0} {:.0}",
                me.x, me.y, me.z
            ))
        } else {
            Err(format!(
                "could not reach {:.0} {:.0} {:.0}; stopped at {:.0} {:.0} {:.0}, {:.0} blocks away",
                target.x,
                target.y,
                target.z,
                me.x,
                me.y,
                me.z,
                me.distance_to(target)
            ))
        }
    }

    pub async fn goto(&self, x: i32, y: i32, z: i32) -> ActionResult {
        let target = BlockPos::new(x, y, z).center();
        let distance = self.position().distance_to(target);
        let timeout = Duration::from_secs_f64(15.0 + distance * 0.6);
        self.walk_to(target, 1.5, timeout).await
    }

    pub async fn follow(&self, player: &str, seconds: u64) -> ActionResult {
        let seconds = seconds.clamp(3, 120);
        let deadline = Instant::now() + Duration::from_secs(seconds);
        let mut seen = false;
        while Instant::now() < deadline {
            let Some(target) = self
                .entities(96.0)
                .into_iter()
                .find(|e| e.name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(player)))
            else {
                if !seen {
                    return Err(format!("cannot see a player named {player} within 96 blocks"));
                }
                self.client.wait_ticks(20).await;
                continue;
            };
            seen = true;
            if self.position().distance_to(target.pos) > 3.5 {
                self.client.start_goto(RadiusGoal::new(target.pos, 2.5));
            }
            self.client.wait_ticks(10).await;
        }
        self.client.stop_pathfinding();
        Ok(format!("followed {player} for {seconds}s"))
    }

    /// Mines up to `n` of the nearest blocks matching `block` and picks up
    /// what drops. `block` may be a group name such as `log` or `iron_ore`.
    pub async fn mine(&self, block: &str, n: u32) -> ActionResult {
        let n = n.clamp(1, 64);
        let kinds = world::resolve_blocks(block)?;
        let before = self.inventory_counts();
        let mut mined = 0;
        let mut skipped: Vec<BlockPos> = Vec::new();
        let mut errors = Vec::new();
        while mined < n {
            let Some(pos) = world::nearest_block(&self.client, &kinds, 64, &skipped) else {
                errors.push(format!("no more {block} within 64 blocks"));
                break;
            };
            if let Err(e) = self
                .walk_to(pos.center(), 3.0, Duration::from_secs(30))
                .await
            {
                skipped.push(pos);
                errors.push(e);
                if skipped.len() >= 4 {
                    break;
                }
                continue;
            }
            self.client.look_at(pos.center());
            self.client.wait_ticks(1).await;
            let dig = azalea::auto_tool::AutoToolClientExt::mine_with_auto_tool(&self.client, pos);
            if tokio::time::timeout(Duration::from_secs(20), dig).await.is_err() {
                skipped.push(pos);
                errors.push(format!("took too long to break {block} at {} {} {}", pos.x, pos.y, pos.z));
                continue;
            }
            self.client.wait_ticks(2).await;
            if world::block_name(&self.client, pos).is_some_and(|name| kinds.iter().any(|k| strip_ns(&k.to_string()) == name)) {
                skipped.push(pos);
                errors.push(format!("{block} at {} {} {} did not break", pos.x, pos.y, pos.z));
                continue;
            }
            mined += 1;
            // Drops land where the block was or fall below it.
            let _ = self.walk_to(pos.center(), 0.5, Duration::from_secs(6)).await;
            self.collect_drops(8.0).await;
        }
        let gained = diff(&before, &self.inventory_counts());
        let mut result = json!({"mined": mined, "gained": gained});
        if mined > 0 && gained.is_empty() {
            let needs_pick = block.contains("stone") || block.contains("ore") || block.contains("deepslate");
            result["warning"] = json!(if needs_pick {
                "blocks broke but nothing dropped: stone and ores need a pickaxe"
            } else {
                "blocks broke but the drops were not picked up"
            });
        }
        if mined == 0 {
            return Err(format!(
                "mined 0 {block}: {}",
                errors.into_iter().take(2).collect::<Vec<_>>().join("; ")
            ));
        }
        if !errors.is_empty() {
            result["problems"] = json!(errors.into_iter().take(2).collect::<Vec<_>>());
        }
        Ok(result.to_string())
    }

    /// Walks over dropped items near the bot so they get picked up.
    pub async fn collect_drops(&self, radius: f64) {
        self.client.wait_ticks(8).await;
        for _ in 0..4 {
            let Some(item) = self.entities(radius).into_iter().find(|e| e.kind == "item") else {
                break;
            };
            let _ = self.walk_to(item.pos, 0.5, Duration::from_secs(5)).await;
            self.client.wait_ticks(6).await;
        }
    }

    /// Puts an item into the hotbar and selects it.
    pub async fn hold(&self, matches: impl Fn(&str) -> bool) -> Result<String, String> {
        let menu = self.inventory_menu();
        let slots = menu.slots();
        let find = |range: std::ops::RangeInclusive<usize>| {
            range.into_iter().find(|&i| item_name(&slots[i]).is_some_and(|(n, _)| matches(&n)))
        };
        if let Some(i) = find(36..=44) {
            self.client.set_selected_hotbar_slot((i - 36) as u8);
            self.client.wait_updates(2).await;
            return Ok(item_name(&slots[i]).unwrap().0);
        }
        let Some(i) = find(9..=35) else {
            return Err("not in inventory".into());
        };
        let target = self.client.selected_hotbar_slot();
        self.client.get_inventory().click(SwapClick {
            source_slot: i as u16,
            target_slot: target,
        });
        self.client.wait_ticks(2).await;
        Ok(item_name(&slots[i]).unwrap().0)
    }

    pub async fn place(&self, block: &str, x: i32, y: i32, z: i32) -> ActionResult {
        let block = strip_ns(block);
        if self.count(&block) == 0 {
            return Err(format!("no {block} in inventory"));
        }
        let target = BlockPos::new(x, y, z);
        if !world::is_replaceable(&self.client, target) {
            return Err(format!(
                "{x} {y} {z} is occupied by {}",
                world::block_name(&self.client, target).unwrap_or_else(|| "an unloaded chunk".into())
            ));
        }
        let Some((support, face)) = world::support_face(&self.client, target) else {
            return Err(format!("nothing solid next to {x} {y} {z} to place against"));
        };
        let feet = self.block_pos();
        if feet == target || feet.up(1) == target {
            // Step aside so we are not standing in the target.
            let aside = target.center() + Vec3::new(2.0, 0.0, 0.0);
            let _ = self.walk_to(aside, 1.0, Duration::from_secs(8)).await;
        }
        if self.client.eye_position().distance_to(target.center()) > 4.0 {
            self.walk_to(target.center(), 3.0, Duration::from_secs(30)).await?;
        }
        self.hold(|n| n == block).await.map_err(|e| format!("{block}: {e}"))?;
        self.client.look_at(face);
        self.client.wait_ticks(2).await;
        self.client.block_interact(support);
        self.client.wait_ticks(6).await;
        match world::block_name(&self.client, target) {
            Some(name) if name == block || name.starts_with(block.trim_end_matches("_bed")) => {
                Ok(format!("placed {block} at {x} {y} {z}"))
            }
            other => Err(format!(
                "placing {block} at {x} {y} {z} failed; the block there is {}",
                other.unwrap_or_else(|| "unknown".into())
            )),
        }
    }

    pub async fn craft(&self, item: &str, n: u32) -> ActionResult {
        let item = strip_ns(item);
        let mut made_first = Vec::new();
        if !matches!(item.as_str(), "planks" | "stick") && !item.ends_with("_planks") {
            if let Some(recipe) = recipes::find(&item).first() {
                let crafts = ((n.clamp(1, 64) as i32) + recipe.count - 1) / recipe.count;
                let mut totals = recipe.totals();
                // Sticks use planks, so make them before topping up planks.
                totals.sort_by_key(|(ing, _)| !ing.matches("stick"));
                for (ing, per) in totals {
                    let want = per * crafts;
                    let have: i32 = self.inventory_counts().iter().filter(|(k, _)| ing.matches(k)).map(|(_, v)| *v).sum();
                    if have >= want {
                        continue;
                    }
                    let sub = if ing.matches("oak_planks") {
                        "planks"
                    } else if ing.matches("stick") {
                        "stick"
                    } else {
                        continue;
                    };
                    if sub == "stick" {
                        // Sticks need planks too.
                        let planks: i32 = self.inventory_counts().iter().filter(|(k, _)| k.ends_with("_planks")).map(|(_, v)| *v).sum();
                        if planks < 2 {
                            let _ = Box::pin(self.craft("planks", 4)).await;
                        }
                    }
                    if let Ok(r) = Box::pin(self.craft(sub, (want - have) as u32)).await {
                        made_first.push(r);
                    }
                }
            }
        }
        let result = self.craft_one(&item, n).await;
        match (result, made_first.is_empty()) {
            (Ok(r), false) => Ok(format!("{r} (first made: {})", made_first.join(", "))),
            (r, _) => r,
        }
    }

    async fn craft_one(&self, item: &str, n: u32) -> ActionResult {
        let item = item.to_string();
        let n = n.clamp(1, 64) as i32;
        let candidates = recipes::find(&item);
        if candidates.is_empty() {
            return Err(format!(
                "I don't know a recipe for {item}. Known: planks, stick, crafting_table, chest, furnace, torch, bread, bed, ladder, shield, and wooden/stone/iron/golden/diamond pickaxe, axe, shovel, sword, hoe"
            ));
        }
        let inv = self.inventory_counts();
        // Pick the variant we have the most ingredients for.
        let Some(recipe) = candidates.iter().find(|r| {
            r.totals()
                .iter()
                .all(|(ing, need)| inv.iter().filter(|(k, _)| ing.matches(k)).map(|(_, v)| *v).sum::<i32>() >= *need)
        }) else {
            let r = &candidates[0];
            let needs: Vec<String> = r
                .totals()
                .iter()
                .map(|(ing, k)| format!("{k} {}", ing.describe()))
                .collect();
            return Err(format!("missing ingredients for {}: needs {}", r.output, needs.join(" + ")));
        };
        let crafts = ((n + recipe.count - 1) / recipe.count).max(1);
        let crafts = recipe
            .totals()
            .iter()
            .map(|(ing, need)| inv.iter().filter(|(k, _)| ing.matches(k)).map(|(_, v)| *v).sum::<i32>() / need)
            .min()
            .unwrap_or(0)
            .min(crafts)
            .min(64);
        let before = inv;

        if recipe.needs_table() {
            let table = world::nearest_block(&self.client, &world::resolve_blocks("crafting_table")?, 32, &[]);
            let Some(table) = table else {
                return Err(format!(
                    "{} needs a crafting_table within 32 blocks; craft one (4 planks) and place it",
                    recipe.output
                ));
            };
            self.walk_to(table.center(), 2.5, Duration::from_secs(30)).await?;
            self.client.look_at(table.center());
            let Some(container) = self.client.open_container_at(table).await else {
                return Err("could not open the crafting table".into());
            };
            fill_grid(&self.client, &container_ops(&container), recipe, crafts, 3, 1, 10..=45).await?;
            container.shift_click(0usize);
            self.client.wait_ticks(4).await;
            container.close();
        } else {
            let inv_handle = self.client.get_inventory();
            fill_grid(&self.client, &inv_ops(&inv_handle), recipe, crafts, 2, 1, 9..=44).await?;
            inv_handle.shift_click(0usize);
            self.client.wait_ticks(4).await;
            inv_handle.close();
        }
        self.client.wait_ticks(4).await;
        let gained = diff(&before, &self.inventory_counts());
        let got = gained.get(&recipe.output).copied().unwrap_or(0);
        if got <= 0 {
            return Err(format!("crafting {} produced nothing", recipe.output));
        }
        Ok(json!({"crafted": recipe.output, "count": got}).to_string())
    }

    pub async fn smelt(&self, item: &str, n: u32) -> ActionResult {
        let item = strip_ns(item);
        let Some(output) = recipes::smelt_output(&item) else {
            return Err(format!("{item} cannot be smelted"));
        };
        let have = self.count(&item);
        if have == 0 {
            return Err(format!("no {item} in inventory"));
        }
        let n = (n.clamp(1, 64) as i32).min(have);
        let inv = self.inventory_counts();
        let Some(fuel) = inv.keys().find(|k| recipes::is_fuel(k) && **k != item).cloned() else {
            return Err("no fuel (coal, charcoal, planks or logs) in inventory".into());
        };
        let Some(furnace) = world::nearest_block(&self.client, &world::resolve_blocks("furnace")?, 32, &[]) else {
            return Err("no furnace within 32 blocks; craft one (8 cobblestone) and place it".into());
        };
        self.walk_to(furnace.center(), 2.5, Duration::from_secs(30)).await?;
        let Some(container) = self.client.open_container_at(furnace).await else {
            return Err("could not open the furnace".into());
        };
        let ops = container_ops(&container);
        // Furnace: 0 input, 1 fuel, 2 output, 3..=38 player inventory.
        move_items(&self.client, &ops, &item, n, 0, 3..=38).await?;
        let fuel_needed = if fuel == "coal" || fuel == "charcoal" { (n + 7) / 8 } else { (n * 3 + 1) / 2 };
        move_items(&self.client, &ops, &fuel, fuel_needed, 1, 3..=38).await?;
        let before = self.count(output);
        let deadline = Instant::now() + Duration::from_secs(12 * n as u64 + 10);
        while Instant::now() < deadline {
            self.client.wait_ticks(20).await;
            let slots = (ops.slots)().unwrap_or_default();
            let done = slots.get(2).and_then(item_name).map(|(_, c)| c).unwrap_or(0);
            let left = slots.first().and_then(item_name).map(|(_, c)| c).unwrap_or(0);
            if left == 0 && done > 0 {
                break;
            }
        }
        container.shift_click(2usize);
        container.shift_click(0usize);
        container.shift_click(1usize);
        self.client.wait_ticks(4).await;
        drop(ops);
        container.close();
        self.client.wait_ticks(4).await;
        let got = self.count(output) - before;
        if got <= 0 {
            return Err(format!("smelting {item} produced nothing"));
        }
        Ok(json!({"smelted": item, "into": output, "count": got}).to_string())
    }

    pub async fn eat(&self) -> ActionResult {
        let inv = self.inventory_counts();
        let Some(food) = FOODS.iter().find(|f| inv.contains_key(**f)) else {
            return Err("no food in inventory".into());
        };
        if self.food() >= 20 {
            return Err("not hungry".into());
        }
        let before = self.count(food);
        self.hold(|n| n == *food).await?;
        self.client.start_use_item();
        self.client.wait_ticks(40).await;
        if self.count(food) < before {
            Ok(format!("ate {food}; food is now {}", self.food()))
        } else {
            Err(format!("tried to eat {food} but it was not consumed"))
        }
    }

    /// Fights the nearest hostile (or the given entity type) for a few seconds.
    pub async fn attack(&self, target: Option<&str>) -> ActionResult {
        let pick = |list: Vec<EntityInfo>| {
            list.into_iter().find(|e| match target {
                Some(t) => e.kind == t || e.name.as_deref() == Some(t),
                None => e.hostile,
            })
        };
        let Some(first) = pick(self.entities(16.0)) else {
            return Err(format!("no {} within 16 blocks", target.unwrap_or("hostile mob")));
        };
        let _ = self
            .hold(|n| n.ends_with("_sword") || n.ends_with("_axe"))
            .await;
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut hits = 0;
        while Instant::now() < deadline {
            let Some(e) = self.entities(20.0).into_iter().find(|e| e.entity == first.entity) else {
                return Ok(format!("{} is gone after {hits} hits", first.kind));
            };
            if e.distance > 3.0 {
                self.client.start_goto(RadiusGoal::new(e.pos, 2.0));
                self.client.wait_ticks(5).await;
                continue;
            }
            self.client.look_at(e.pos + Vec3::new(0.0, 1.0, 0.0));
            if self.client.attack_cooldown_remaining_ticks() == 0 {
                self.client.attack(e.entity);
                hits += 1;
            }
            self.client.wait_ticks(1).await;
        }
        self.client.stop_pathfinding();
        Ok(format!("hit {} {hits} times", first.kind))
    }

    pub fn head_in_water(&self) -> bool {
        self.client
            .get_component::<azalea::entity::FluidOnEyes>()
            .is_some_and(|f| format!("{:?}", *f).to_lowercase().contains("water"))
    }

    /// Gets out of whatever is hurting us without an attacker: swims up when
    /// underwater, otherwise jumps and steps aside (fire, cactus, a wall).
    pub async fn escape_hazard(&self) -> ActionResult {
        self.client.stop_pathfinding();
        let underwater = self.head_in_water();
        self.client.set_jumping(true);
        for _ in 0..60 {
            self.client.wait_ticks(1).await;
            if underwater && !self.head_in_water() {
                break;
            }
        }
        self.client.set_jumping(false);
        if !underwater {
            let me = self.position();
            let _ = self.walk_to(me + Vec3::new(3.0, 0.0, 3.0), 1.5, Duration::from_secs(5)).await;
        }
        Ok(if underwater { "swam up for air".into() } else { "jumped clear".into() })
    }

    /// Runs away from the nearest hostiles for a few seconds.
    pub async fn flee(&self) -> ActionResult {
        let threats = self.hostiles(16.0);
        if threats.is_empty() {
            return Ok("nothing to flee from".into());
        }
        let me = self.position();
        let mut away = Vec3::new(0.0, 0.0, 0.0);
        for t in &threats {
            let d = me - t.pos;
            away = away + Vec3::new(d.x, 0.0, d.z).normalize();
        }
        let away = if away.length() < 0.01 { Vec3::new(1.0, 0.0, 0.0) } else { away.normalize() };
        let target = me + away * 16.0;
        let _ = self.walk_to(target, 3.0, Duration::from_secs(8)).await;
        Ok(format!("fled from {}", threats[0].kind))
    }

    /// Finds a bed, walks to it and lies down. Fails if it is not night or
    /// monsters are close, which is exactly what the game would refuse.
    pub async fn sleep(&self) -> ActionResult {
        if self.is_sleeping() {
            return Ok("already sleeping".into());
        }
        let time = self.time();
        let thundering = self.senses.lock().unwrap().thundering;
        if !time.can_sleep() && !thundering {
            return Err(format!("not night (it is {}); beds work only at night", time.describe()));
        }
        let beds = world::resolve_blocks("bed")?;
        let preferred = self.senses.lock().unwrap().bed;
        let bed = preferred
            .filter(|b| world::block_name(&self.client, *b).is_some_and(|n| n.ends_with("_bed")))
            .or_else(|| world::nearest_block(&self.client, &beds, 128, &[]));
        let Some(bed) = bed else {
            return Err("no bed within 128 blocks; craft one (3 wool of one color + 3 planks) and place it".into());
        };
        let distance = self.position().distance_to(bed.center());
        self.walk_to(bed.center(), 1.5, Duration::from_secs_f64(20.0 + distance * 0.6))
            .await
            .map_err(|e| format!("bed at {} {} {}: {e}", bed.x, bed.y, bed.z))?;
        let near: Vec<String> = self
            .hostiles(10.0)
            .iter()
            .map(|e| format!("{} {:.0}m", e.kind, e.distance))
            .collect();
        if !near.is_empty() {
            return Err(format!("monsters nearby ({}); deal with them first", near.join(", ")));
        }
        let asked = Instant::now();
        self.client.look_at(bed.center());
        self.client.wait_ticks(1).await;
        self.client.block_interact(bed);
        for _ in 0..40 {
            self.client.wait_ticks(1).await;
            if self.is_sleeping() {
                self.senses.lock().unwrap().bed = Some(bed);
                return Ok(format!("in bed at {} {} {}", bed.x, bed.y, bed.z));
            }
        }
        let why = self.system_messages_since(asked);
        Err(match why.last() {
            Some(msg) => format!("could not sleep: {msg}"),
            None => "could not get into the bed".into(),
        })
    }

    pub async fn wake_up(&self) {
        if self.is_sleeping() {
            use azalea::protocol::packets::game::s_player_command::{Action, ServerboundPlayerCommand};
            let id = self.client.minecraft_entity_id_of_self();
            self.client.write_packet(ServerboundPlayerCommand {
                id,
                action: Action::StopSleeping,
                data: 0,
            });
        }
    }
}

trait SelfId {
    fn minecraft_entity_id_of_self(&self) -> azalea::world::MinecraftEntityId;
}

impl SelfId for Client {
    fn minecraft_entity_id_of_self(&self) -> azalea::world::MinecraftEntityId {
        self.minecraft_entity_by_ecs_entity(self.entity)
            .unwrap_or(azalea::world::MinecraftEntityId(0))
    }
}

/// Distance to the bed we last slept in, or the nearest bed we can see.
pub fn nearest_bed_distance(body: &Body) -> Option<f64> {
    let known = body.senses.lock().unwrap().bed;
    let bed = known.or_else(|| {
        let beds = world::resolve_blocks("bed").ok()?;
        world::nearest_block(&body.client, &beds, 128, &[])
    })?;
    Some(body.position().distance_to(bed.center()))
}

pub fn diff(before: &BTreeMap<String, i32>, after: &BTreeMap<String, i32>) -> BTreeMap<String, i32> {
    let mut out = BTreeMap::new();
    for (k, v) in after {
        let d = v - before.get(k).copied().unwrap_or(0);
        if d != 0 {
            out.insert(k.clone(), d);
        }
    }
    for (k, v) in before {
        if !after.contains_key(k) {
            out.insert(k.clone(), -v);
        }
    }
    out
}

/// Click operations on either the player inventory or an open container.
struct GridOps<'a> {
    click: Box<dyn Fn(ClickOperation) + Send + Sync + 'a>,
    slots: Box<dyn Fn() -> Option<Vec<ItemStack>> + Send + Sync + 'a>,
}

fn container_ops(c: &azalea::container::ContainerHandle) -> GridOps<'_> {
    GridOps {
        click: Box::new(move |op| c.click(op)),
        slots: Box::new(move || c.slots()),
    }
}

fn inv_ops(c: &azalea::container::ContainerHandleRef) -> GridOps<'_> {
    GridOps {
        click: Box::new(move |op| c.click(op)),
        slots: Box::new(move || c.slots()),
    }
}

/// Puts `crafts` items into each recipe cell of a crafting grid.
async fn fill_grid(
    client: &Client,
    ops: &GridOps<'_>,
    recipe: &recipes::Recipe,
    crafts: i32,
    grid_width: usize,
    grid_start: usize,
    inventory: std::ops::RangeInclusive<usize>,
) -> Result<(), String> {
    for row in 0..recipe.height {
        for col in 0..recipe.width {
            let Some(ingredient) = &recipe.cells[row * recipe.width + col] else {
                continue;
            };
            let grid_slot = (grid_start + row * grid_width + col) as u16;
            let mut placed = 0;
            while placed < crafts {
                let slots = (ops.slots)().ok_or("container closed")?;
                let Some(source) = inventory.clone().find(|&i| {
                    item_name(&slots[i]).is_some_and(|(n, _)| ingredient.matches(&n))
                }) else {
                    return Err(format!("ran out of {}", ingredient.describe()));
                };
                let available = item_name(&slots[source]).map(|(_, c)| c).unwrap_or(0);
                (ops.click)(PickupClick::Left { slot: Some(source as u16) }.into());
                client.wait_ticks(1).await;
                let take = available.min(crafts - placed);
                for _ in 0..take {
                    (ops.click)(PickupClick::Right { slot: Some(grid_slot) }.into());
                }
                (ops.click)(PickupClick::Left { slot: Some(source as u16) }.into());
                client.wait_ticks(1).await;
                placed += take;
            }
        }
    }
    client.wait_ticks(3).await;
    Ok(())
}

/// Moves `n` of an item from the player part of a container into `slot`.
async fn move_items(
    client: &Client,
    ops: &GridOps<'_>,
    item: &str,
    n: i32,
    slot: u16,
    inventory: std::ops::RangeInclusive<usize>,
) -> Result<(), String> {
    let mut moved = 0;
    while moved < n {
        let slots = (ops.slots)().ok_or("container closed")?;
        let Some(source) = inventory
            .clone()
            .find(|&i| item_name(&slots[i]).is_some_and(|(name, _)| name == item))
        else {
            break;
        };
        let available = item_name(&slots[source]).map(|(_, c)| c).unwrap_or(0);
        let take = available.min(n - moved);
        (ops.click)(PickupClick::Left { slot: Some(source as u16) }.into());
        client.wait_ticks(1).await;
        if take == available {
            (ops.click)(PickupClick::Left { slot: Some(slot) }.into());
        } else {
            for _ in 0..take {
                (ops.click)(PickupClick::Right { slot: Some(slot) }.into());
            }
            (ops.click)(PickupClick::Left { slot: Some(source as u16) }.into());
        }
        client.wait_ticks(1).await;
        moved += take;
    }
    if moved == 0 {
        return Err(format!("no {item} to move"));
    }
    Ok(())
}
