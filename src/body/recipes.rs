//! Built-in crafting recipes.
//!
//! azalea has no crafting API, and its place-recipe packet does not match the
//! 1.21.11 protocol. So the body crafts the way a player does: it clicks
//! ingredients into the grid and shift-clicks the result. Vanilla allows
//! that for any recipe, unlocked or not. This table holds the recipes a bot
//! needs to get from a tree to an iron pickaxe and a bed.

/// One grid cell. `Any` lists the item names that fit, e.g. any planks.
#[derive(Debug, Clone, PartialEq)]
pub enum Ingredient {
    Item(String),
    Any(Vec<String>),
}

impl Ingredient {
    pub fn matches(&self, item: &str) -> bool {
        match self {
            Ingredient::Item(name) => name == item,
            Ingredient::Any(names) => names.iter().any(|n| n == item),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Ingredient::Item(name) => name.clone(),
            Ingredient::Any(names) if names.iter().all(|n| n.ends_with("_planks")) => "any planks".into(),
            Ingredient::Any(names) if names.iter().all(|n| n.ends_with("_log") || n.ends_with("_wood")) => {
                "any log".into()
            }
            Ingredient::Any(names) => format!("one of {}", names.join("/")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Recipe {
    pub output: String,
    pub count: i32,
    pub width: usize,
    pub height: usize,
    /// Row-major, `width * height` cells.
    pub cells: Vec<Option<Ingredient>>,
}

impl Recipe {
    pub fn needs_table(&self) -> bool {
        self.width > 2 || self.height > 2
    }

    /// Total of each ingredient for one craft.
    pub fn totals(&self) -> Vec<(Ingredient, i32)> {
        let mut totals: Vec<(Ingredient, i32)> = Vec::new();
        for cell in self.cells.iter().flatten() {
            match totals.iter_mut().find(|(i, _)| i == cell) {
                Some((_, n)) => *n += 1,
                None => totals.push((cell.clone(), 1)),
            }
        }
        totals
    }
}

pub const WOODS: [&str; 9] = [
    "oak", "spruce", "birch", "jungle", "acacia", "dark_oak", "mangrove", "cherry", "pale_oak",
];
pub const COLORS: [&str; 16] = [
    "white", "orange", "magenta", "light_blue", "yellow", "lime", "pink", "gray", "light_gray",
    "cyan", "purple", "blue", "brown", "green", "red", "black",
];

fn planks() -> Ingredient {
    Ingredient::Any(WOODS.iter().map(|w| format!("{w}_planks")).collect())
}

fn stone() -> Ingredient {
    Ingredient::Any(vec![
        "cobblestone".into(),
        "cobbled_deepslate".into(),
        "blackstone".into(),
    ])
}

fn item(name: &str) -> Ingredient {
    Ingredient::Item(name.into())
}

/// Builds a recipe from rows such as `["PPP", " S ", " S "]`.
fn shaped(output: &str, count: i32, rows: &[&str], key: &[(char, Ingredient)]) -> Recipe {
    let height = rows.len();
    let width = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut cells = Vec::with_capacity(width * height);
    for row in rows {
        for c in 0..width {
            let ch = row.chars().nth(c).unwrap_or(' ');
            cells.push(key.iter().find(|(k, _)| *k == ch).map(|(_, i)| i.clone()));
        }
    }
    Recipe {
        output: output.into(),
        count,
        width,
        height,
        cells,
    }
}

pub fn all() -> Vec<Recipe> {
    let mut recipes = Vec::new();
    for wood in WOODS {
        recipes.push(shaped(
            &format!("{wood}_planks"),
            4,
            &["L"],
            &[(
                'L',
                Ingredient::Any(vec![
                    format!("{wood}_log"),
                    format!("{wood}_wood"),
                    format!("stripped_{wood}_log"),
                ]),
            )],
        ));
    }
    for color in COLORS {
        recipes.push(shaped(
            &format!("{color}_bed"),
            1,
            &["WWW", "PPP"],
            &[('W', item(&format!("{color}_wool"))), ('P', planks())],
        ));
    }
    let p = || ('P', planks());
    let s = || ('S', item("stick"));
    recipes.push(shaped("stick", 4, &["P", "P"], &[p()]));
    recipes.push(shaped("crafting_table", 1, &["PP", "PP"], &[p()]));
    recipes.push(shaped("chest", 1, &["PPP", "P P", "PPP"], &[p()]));
    recipes.push(shaped("furnace", 1, &["CCC", "C C", "CCC"], &[('C', stone())]));
    recipes.push(shaped(
        "torch",
        4,
        &["C", "S"],
        &[('C', Ingredient::Any(vec!["coal".into(), "charcoal".into()])), s()],
    ));
    recipes.push(shaped("bread", 1, &["WWW"], &[('W', item("wheat"))]));
    recipes.push(shaped("bowl", 4, &["P P", " P "], &[p()]));
    recipes.push(shaped("ladder", 3, &["S S", "SSS", "S S"], &[s()]));
    recipes.push(shaped(
        "shield",
        1,
        &["PIP", "PPP", " P "],
        &[p(), ('I', item("iron_ingot"))],
    ));
    recipes.push(shaped("iron_block", 1, &["III", "III", "III"], &[('I', item("iron_ingot"))]));

    for (tier, material) in [
        ("wooden", planks()),
        ("stone", stone()),
        ("iron", item("iron_ingot")),
        ("golden", item("gold_ingot")),
        ("diamond", item("diamond")),
    ] {
        let m = || ('M', material.clone());
        recipes.push(shaped(&format!("{tier}_pickaxe"), 1, &["MMM", " S ", " S "], &[m(), s()]));
        recipes.push(shaped(&format!("{tier}_axe"), 1, &["MM", "MS", " S"], &[m(), s()]));
        recipes.push(shaped(&format!("{tier}_shovel"), 1, &["M", "S", "S"], &[m(), s()]));
        recipes.push(shaped(&format!("{tier}_sword"), 1, &["M", "M", "S"], &[m(), s()]));
        recipes.push(shaped(&format!("{tier}_hoe"), 1, &["MM", " S", " S"], &[m(), s()]));
    }
    recipes
}

/// Finds the recipe for an item. `planks` and `bed` match any wood or color;
/// the caller picks the variant it has ingredients for.
pub fn find(item: &str) -> Vec<Recipe> {
    let item = item.trim_start_matches("minecraft:");
    let found = find_exact(item);
    match item.strip_suffix('s') {
        // Models write plurals: "sticks", "chests", "beds".
        Some(singular) if found.is_empty() => find_exact(singular),
        _ => found,
    }
}

fn find_exact(item: &str) -> Vec<Recipe> {
    all()
        .into_iter()
        .filter(|r| {
            r.output == item
                || (item == "planks" && r.output.ends_with("_planks"))
                || (item == "bed" && r.output.ends_with("_bed"))
        })
        .collect()
}

/// Furnace inputs and what they smelt into.
pub fn smelt_output(input: &str) -> Option<&'static str> {
    Some(match input {
        "raw_iron" | "iron_ore" | "deepslate_iron_ore" => "iron_ingot",
        "raw_gold" | "gold_ore" | "deepslate_gold_ore" => "gold_ingot",
        "raw_copper" => "copper_ingot",
        "sand" => "glass",
        "cobblestone" => "stone",
        "beef" => "cooked_beef",
        "porkchop" => "cooked_porkchop",
        "chicken" => "cooked_chicken",
        "mutton" => "cooked_mutton",
        "cod" => "cooked_cod",
        "salmon" => "cooked_salmon",
        "potato" => "baked_potato",
        "rabbit" => "cooked_rabbit",
        _ if input.ends_with("_log") => "charcoal",
        _ => return None,
    })
}

pub const FUELS: [&str; 6] = ["coal", "charcoal", "coal_block", "stick", "oak_planks", "spruce_planks"];

pub fn is_fuel(item: &str) -> bool {
    FUELS.contains(&item) || item.ends_with("_planks") || item.ends_with("_log")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pickaxe_shape() {
        let r = &find("wooden_pickaxe")[0];
        assert_eq!((r.width, r.height), (3, 3));
        assert!(r.needs_table());
        let totals = r.totals();
        assert_eq!(totals.len(), 2);
        assert_eq!(totals.iter().map(|(_, n)| n).sum::<i32>(), 5);
    }

    #[test]
    fn planks_fit_inventory_grid() {
        let r = &find("oak_planks")[0];
        assert!(!r.needs_table());
        assert!(r.cells[0].as_ref().unwrap().matches("oak_log"));
        assert_eq!(find("planks").len(), WOODS.len());
    }

    #[test]
    fn axe_is_mirrored_correctly() {
        let r = &find("stone_axe")[0];
        assert_eq!((r.width, r.height), (2, 3));
        assert!(r.cells[0].is_some() && r.cells[1].is_some());
        assert!(r.cells[2].is_some() && r.cells[3].is_some());
        assert!(r.cells[4].is_none() && r.cells[5].is_some());
    }
}
