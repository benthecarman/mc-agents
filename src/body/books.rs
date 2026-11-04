//! Written books in chests: how a bot leaves knowledge for the next one.
//!
//! Writing signs a book and quill held in the hotbar with
//! `ServerboundEditBook`, then shift-clicks the written book into the nearest
//! chest. Reading opens nearby chests and reads `WrittenBookContent` straight
//! from the item data, so the book stays where it is for others.

use std::time::Duration;

use azalea::{
    container::ContainerClientExt, prelude::*, protocol::packets::game::s_edit_book::ServerboundEditBook, BlockPos,
};
use azalea_inventory::{components::WrittenBookContent, ItemStack};

use super::{world, ActionResult, Body};

/// Vanilla allows 100 pages of 1024 characters; stay under both.
const PAGE_CHARS: usize = 1000;
const MAX_PAGES: usize = 100;

pub struct Book {
    pub title: String,
    pub author: String,
    pub text: String,
    pub chest: Option<BlockPos>,
}

/// Splits text into pages, preferring line breaks.
pub fn paginate(text: &str) -> Vec<String> {
    let mut pages = Vec::new();
    let mut page = String::new();
    for line in text.split_inclusive('\n') {
        let mut line = line;
        while !line.is_empty() {
            let room = PAGE_CHARS - page.chars().count();
            if line.chars().count() <= room {
                page.push_str(line);
                break;
            }
            if !page.is_empty() {
                pages.push(std::mem::take(&mut page));
                continue;
            }
            // A single line longer than a page: cut it.
            let cut = line.char_indices().nth(PAGE_CHARS).map(|(i, _)| i).unwrap_or(line.len());
            pages.push(line[..cut].to_string());
            line = &line[cut..];
        }
    }
    if !page.is_empty() {
        pages.push(page);
    }
    pages
}

fn book_of(stack: &ItemStack) -> Option<WrittenBookContent> {
    match stack {
        ItemStack::Present(data) if data.kind.to_string().ends_with("written_book") => {
            data.get_component::<WrittenBookContent>().map(|c| c.into_owned())
        }
        _ => None,
    }
}

fn read(content: &WrittenBookContent, chest: Option<BlockPos>) -> Book {
    Book {
        title: content.title.raw.clone(),
        author: content.author.clone(),
        text: content.pages.iter().map(|p| p.raw.to_string()).collect::<Vec<_>>().join(""),
        chest,
    }
}

impl Body {
    /// Signs a book and quill with `text` and stores it in the nearest chest.
    pub async fn store_book(&self, title: &str, text: &str) -> ActionResult {
        if self.count("writable_book") == 0 {
            return Err("no writable_book (book and quill) in inventory; craft one from a book, an ink_sac and a feather".into());
        }
        let pages = paginate(text);
        if pages.len() > MAX_PAGES {
            return Err(format!("too much to write: {} pages, the limit is {MAX_PAGES}", pages.len()));
        }
        let chests = world::resolve_blocks("chest")?;
        let Some(chest) = world::nearest_block(&self.client, &chests, 32, &[]) else {
            return Err("no chest within 32 blocks; craft one (8 planks) and place it first".into());
        };
        self.hold(|n| n == "writable_book").await?;
        let before = self.count("written_book");
        let title: String = title.chars().take(32).collect();
        self.client.write_packet(ServerboundEditBook {
            slot: self.client.selected_hotbar_slot() as u32,
            pages: pages.clone(),
            title: Some(title.clone()),
        });
        let mut signed = false;
        for _ in 0..40 {
            self.client.wait_ticks(1).await;
            if self.count("written_book") > before {
                signed = true;
                break;
            }
        }
        if !signed {
            return Err("the server did not sign the book".into());
        }
        self.walk_to(chest.center(), 2.5, Duration::from_secs(30))
            .await
            .map_err(|e| format!("signed the book but could not reach the chest at {} {} {}: {e}", chest.x, chest.y, chest.z))?;
        let Some(container) = self.client.open_container_at(chest).await else {
            return Err("signed the book but could not open the chest".into());
        };
        let size = container.contents().map(|c| c.len()).unwrap_or(0);
        let slots = container.slots().unwrap_or_default();
        let Some(slot) = (size..slots.len()).find(|&i| book_of(&slots[i]).is_some_and(|b| b.title.raw == title)) else {
            container.close();
            return Err("signed the book but lost track of it".into());
        };
        container.shift_click(slot);
        self.client.wait_ticks(4).await;
        let stored = container
            .contents()
            .unwrap_or_default()
            .iter()
            .any(|s| book_of(s).is_some_and(|b| b.title.raw == title));
        container.close();
        self.client.wait_ticks(2).await;
        if !stored {
            return Err("the chest is full; the book is still in my inventory".into());
        }
        Ok(format!(
            "wrote \"{title}\" ({} pages) and put it in the chest at {} {} {}",
            pages.len(),
            chest.x,
            chest.y,
            chest.z
        ))
    }

    /// Reads every written book in the inventory and in up to 4 chests nearby.
    pub async fn read_books(&self) -> Result<Vec<Book>, String> {
        let mut books: Vec<Book> = self
            .inventory_menu()
            .slots()
            .iter()
            .filter_map(book_of)
            .map(|b| read(&b, None))
            .collect();
        let chests = world::resolve_blocks("chest")?;
        let mut seen = Vec::new();
        for _ in 0..4 {
            let Some(chest) = world::nearest_block(&self.client, &chests, 32, &seen) else {
                break;
            };
            seen.push(chest);
            if self.walk_to(chest.center(), 2.5, Duration::from_secs(30)).await.is_err() {
                continue;
            }
            let Some(container) = self.client.open_container_at(chest).await else {
                continue;
            };
            self.client.wait_ticks(2).await;
            for stack in container.contents().unwrap_or_default() {
                if let Some(book) = book_of(&stack) {
                    books.push(read(&book, Some(chest)));
                }
            }
            container.close();
            self.client.wait_ticks(2).await;
        }
        if books.is_empty() && seen.is_empty() {
            return Err("no written books in my inventory and no chest within 32 blocks".into());
        }
        Ok(books)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_stay_under_the_limit_and_keep_text() {
        let text = format!("{}\n{}\nshort\n", "a".repeat(600), "b".repeat(2500));
        let pages = paginate(&text);
        assert!(pages.iter().all(|p| p.chars().count() <= PAGE_CHARS));
        assert_eq!(pages.concat(), text);
    }
}
