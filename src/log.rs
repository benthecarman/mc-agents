//! Console and file logging for everything a bot does.
//!
//! Each line goes to the console in color and, without color, to
//! `logs/all.log` and `logs/<bot>.log`. Never pass secrets to these functions.

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

use owo_colors::OwoColorize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Info,
    Event,
    Reflex,
    Think,
    Plan,
    Tool,
    Result,
    Fail,
    Chat,
    Skill,
    Memory,
    Dream,
    Death,
    Llm,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Info => "info",
            Kind::Event => "event",
            Kind::Reflex => "reflex",
            Kind::Think => "think",
            Kind::Plan => "plan",
            Kind::Tool => "tool",
            Kind::Result => "result",
            Kind::Fail => "fail",
            Kind::Chat => "chat",
            Kind::Skill => "skill",
            Kind::Memory => "memory",
            Kind::Dream => "dream",
            Kind::Death => "death",
            Kind::Llm => "llm",
        }
    }
}

struct Files {
    dir: PathBuf,
    all: File,
    per_bot: HashMap<String, File>,
}

static FILES: OnceLock<Mutex<Files>> = OnceLock::new();

/// Opens `logs/`. Call once at startup. Logging works without it (console only).
pub fn init(dir: impl Into<PathBuf>) -> std::io::Result<()> {
    let dir = dir.into();
    std::fs::create_dir_all(&dir)?;
    let all = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("all.log"))?;
    let _ = FILES.set(Mutex::new(Files {
        dir,
        all,
        per_bot: HashMap::new(),
    }));
    Ok(())
}

fn bot_color(bot: &str) -> owo_colors::AnsiColors {
    use owo_colors::AnsiColors::*;
    const COLORS: [owo_colors::AnsiColors; 6] =
        [BrightCyan, BrightMagenta, BrightYellow, BrightGreen, BrightBlue, BrightRed];
    let hash = bot.bytes().fold(0usize, |h, b| h.wrapping_mul(31).wrapping_add(b as usize));
    COLORS[hash % COLORS.len()]
}

pub fn log(bot: &str, kind: Kind, text: impl AsRef<str>) {
    let text = text.as_ref();
    let time = chrono::Local::now().format("%-I:%M:%S %p");
    let label = format!("{:<6}", kind.label());
    let colored_label = match kind {
        Kind::Info => label.dimmed().to_string(),
        Kind::Event => label.blue().to_string(),
        Kind::Reflex => label.bright_red().bold().to_string(),
        Kind::Think => label.magenta().to_string(),
        Kind::Plan => label.bright_magenta().bold().to_string(),
        Kind::Tool => label.cyan().to_string(),
        Kind::Result => label.green().to_string(),
        Kind::Fail => label.red().bold().to_string(),
        Kind::Chat => label.bright_white().bold().to_string(),
        Kind::Skill => label.bright_green().bold().to_string(),
        Kind::Memory => label.yellow().to_string(),
        Kind::Dream => label.bright_blue().bold().to_string(),
        Kind::Death => label.on_red().white().bold().to_string(),
        Kind::Llm => label.dimmed().to_string(),
    };
    println!(
        "{} {} {} {}",
        time.dimmed(),
        format!("{bot:<8}").color(bot_color(bot)).bold(),
        colored_label,
        text
    );

    if let Some(files) = FILES.get() {
        let mut files = files.lock().unwrap();
        let line = format!("{time} {bot:<8} {label} {text}\n");
        let _ = files.all.write_all(line.as_bytes());
        let path = files.dir.join(format!("{bot}.log"));
        let file = files.per_bot.entry(bot.to_string()).or_insert_with(|| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .expect("open bot log")
        });
        let _ = file.write_all(line.as_bytes());
    }
}
