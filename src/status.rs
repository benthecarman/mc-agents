//! A board every bot writes to: what it is doing and what it carries.
//!
//! The nameplate task shows the first part over each bot's head, and the god
//! reads the second part to judge challenges.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

use crate::{
    console::Console,
    log::{self, Kind},
};

#[derive(Clone, Debug, Default)]
pub struct Status {
    /// A few words for the nameplate; empty when idle.
    pub doing: String,
    pub items: BTreeMap<String, i32>,
}

static BOARD: LazyLock<Mutex<HashMap<String, Status>>> = LazyLock::new(Default::default);

pub fn update(bot: &str, doing: String, items: BTreeMap<String, i32>) {
    BOARD.lock().unwrap().insert(bot.to_string(), Status { doing, items });
}

pub fn get(bot: &str) -> Option<Status> {
    BOARD.lock().unwrap().get(bot).cloned()
}

/// How many of `item` the counts hold. `log` matches every kind of log,
/// `planks` every kind of planks, and so on.
pub fn count_matching(items: &BTreeMap<String, i32>, item: &str) -> i32 {
    let suffix = format!("_{item}");
    items
        .iter()
        .filter(|(name, _)| *name == item || name.ends_with(&suffix))
        .map(|(_, n)| *n)
        .sum()
}

/// Keeps text safe to put inside a quoted command argument.
fn clean(text: &str, max: usize) -> String {
    let text: String = text
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || " .,!?-'()+:/".contains(*c))
        .collect();
    let text = text.trim();
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}...", text.chars().take(max - 3).collect::<String>().trim_end())
    }
}

/// Shows what each bot is doing next to its name, over its head and in the
/// tab list. Each bot gets a team, and the team's suffix is the text.
pub async fn nameplates(console: Console, bots: Vec<String>) {
    for bot in &bots {
        let team = team(bot);
        let _ = console.run(&format!("team add {team}")).await;
        let _ = console.run(&format!("team join {team} {bot}")).await;
        let _ = console.run(&format!("team modify {team} suffix \"\"")).await;
    }
    let mut shown: HashMap<String, (String, Instant)> = HashMap::new();
    loop {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        for bot in &bots {
            let doing = get(bot).map(|s| clean(&s.doing, 32)).unwrap_or_default();
            // Re-send now and then: a bot that rejoins is not on its team.
            let stale = shown.get(bot).is_none_or(|(_, at)| at.elapsed() > Duration::from_secs(60));
            if !stale && shown.get(bot).is_some_and(|(text, _)| *text == doing) {
                continue;
            }
            let team = team(bot);
            let suffix = if doing.is_empty() {
                "\"\"".to_string()
            } else {
                format!("{{\"text\":\" [{doing}]\",\"color\":\"gray\"}}")
            };
            if stale {
                let _ = console.run(&format!("team join {team} {bot}")).await;
            }
            if let Err(e) = console.run(&format!("team modify {team} suffix {suffix}")).await {
                log::log("world", Kind::Fail, format!("nameplate for {bot}: {e:#}"));
            }
            shown.insert(bot.clone(), (doing, Instant::now()));
        }
    }
}

fn team(bot: &str) -> String {
    format!("bot_{}", bot.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_match_by_suffix() {
        let items = BTreeMap::from([("oak_log".to_string(), 3), ("birch_log".to_string(), 2), ("log_x".to_string(), 9)]);
        assert_eq!(count_matching(&items, "log"), 5);
        assert_eq!(count_matching(&items, "oak_log"), 3);
    }

    #[test]
    fn clean_strips_quotes_and_shortens() {
        assert_eq!(clean("say \"hi\"\\ now", 40), "say hi now");
        assert_eq!(clean("abcdefghijklmnop", 10), "abcdefg...");
    }
}
