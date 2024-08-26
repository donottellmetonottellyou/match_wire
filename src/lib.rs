pub mod cli;
pub mod not_your_private_keys;
pub mod commands {
    pub mod filter;
}
pub mod utils {
    pub mod caching;
    pub mod json_data;
    pub mod subscriber;
}

use std::{
    collections::HashSet,
    io,
    path::{Path, PathBuf},
    time::Duration,
};
use utils::json_data::Version;

pub const VERSION_URL: &str =
    "https://gist.githubusercontent.com/WardLordRuby/324d7c1fb454aed5f5155a790bd028f0/raw/";

pub const H2M_MAX_CLIENT_NUM: i64 = 18;
pub const H2M_MAX_TEAM_SIZE: i64 = 9;

pub const REQUIRED_FILES: [&str; 3] = ["h1_mp64_ship.exe", "h2m-mod", "players2"];

pub const LOCAL_DATA: &str = "LOCALAPPDATA";
pub const CACHED_DATA: &str = "region_cache.json";

pub const APP_NAME: &str = "h2m_favorites";
pub const LOG_NAME: &str = "h2m_favorties.log";

// MARK: TODOS
// 1. need to rework the program to stay open so we can store a map in memory - needs cleanup / mostly done
// 2. grab tracing and set up tracing as well as our map save to file - DONE
// 3. create map of host-name -> (ip:port, region) - DONE
//    - need a map when user filters favorites
//    - need a map when starts the app
// 4. app will have additional command so all current args need to get moved into a new filter command - DONE
// 5. app needs to display a '>' when waiting for a user to enter the next command - DONE
// 6. app needs to be able to interact with h2m-mod.exe or h2m-revivied.exe
//    - listen to the stdout
//    - inject a connect command if it is not busy
// 7. make splash screen for startup

#[macro_export]
macro_rules! new_io_error {
    ($kind:expr, $msg:expr) => {
        Err(io::Error::new($kind, $msg))
    };
}

pub async fn get_latest_version() -> reqwest::Result<()> {
    let current_version = env!("CARGO_PKG_VERSION");
    let client = reqwest::Client::new();
    let version = client
        .get(VERSION_URL)
        .timeout(Duration::from_secs(6))
        .send()
        .await?
        .json::<Version>()
        .await?;
    if current_version != version.latest {
        println!(
            "New version available for download at: \n\
            https://github.com/WardLordRuby/H2M_favorites/releases/download/v{}/h2m_favorites.exe",
            version.latest
        )
    }
    Ok(())
}

#[derive(Debug)]
pub enum Operation {
    All,
    Any,
    Count,
}

pub enum OperationResult<'a> {
    Bool(bool),
    Count((usize, HashSet<&'a str>)),
}

/// `Operation::All` and `Operation::Any` map to `OperationResult::bool(_result_)`  
/// `Operation::Count` maps to `OperationResult::Count((_num_found_, _HashSet<_&input_list_>))`  
/// when matching you will always have to `_ => unreachable()` for the return type you will never get
pub fn does_dir_contain<'a, T>(
    dir: &Path,
    operation: Operation,
    list: &'a [T],
) -> io::Result<OperationResult<'a>>
where
    T: std::borrow::Borrow<str> + std::cmp::Eq + std::hash::Hash,
{
    let entries = std::fs::read_dir(dir)?;
    let file_names = entries
        .filter_map(|entry| Some(entry.ok()?.file_name()))
        .collect::<Vec<_>>();
    let str_names = file_names
        .iter()
        .filter_map(|f| f.to_str())
        .collect::<HashSet<_>>();

    match operation {
        Operation::All => Ok(OperationResult::Bool({
            let result = list
                .iter()
                .all(|check_file| str_names.contains(check_file.borrow()));
            result
        })),
        Operation::Any => Ok(OperationResult::Bool({
            let result = list
                .iter()
                .any(|check_file| str_names.contains(check_file.borrow()));
            result
        })),
        Operation::Count => Ok(OperationResult::Count({
            let collection = list
                .iter()
                .filter(|&check_file| str_names.contains(check_file.borrow()))
                .map(|t| t.borrow())
                .collect::<HashSet<_>>();
            let num_found = collection.len();
            (num_found, collection)
        })),
    }
}

/// Validates local/app_dir exists and modifies input if valid
pub fn check_app_dir_exists(local: &mut PathBuf) -> io::Result<()> {
    use crate::{does_dir_contain, Operation, OperationResult, APP_NAME};
    match does_dir_contain(local, Operation::All, &[APP_NAME]) {
        Ok(OperationResult::Bool(true)) => {
            local.push(APP_NAME);
            Ok(())
        }
        Ok(OperationResult::Bool(false)) => {
            local.push(APP_NAME);
            std::fs::create_dir(local)
        }
        Err(err) => Err(err),
        _ => unreachable!(),
    }
}

pub fn format_panic_info(info: &std::panic::PanicInfo) -> String {
    let payload_str = if let Some(location) = info.location() {
        format!(
            "PANIC {}:{}:{}:",
            location.file(),
            location.line(),
            location.column(),
        )
    } else {
        String::from("PANIC:")
    };
    if let Some(msg) = info.payload().downcast_ref::<&str>() {
        format!("{payload_str} {msg}")
    } else if let Some(msg) = info.payload().downcast_ref::<String>() {
        format!("{payload_str} {msg}")
    } else {
        format!("{payload_str} no attached message")
    }
}

pub fn lowercase_vec(vec: &[String]) -> Vec<String> {
    vec.iter().map(|s| s.trim().to_lowercase()).collect()
}

pub fn parse_hostname(name: &str) -> String {
    const COLOR_ESCAPE_CODE: char = '^';
    let mut host_name = String::new();
    let mut chars = name.chars().peekable();
    while let Some(c) = chars.next() {
        if c == COLOR_ESCAPE_CODE {
            if chars.peek().is_some() {
                chars.next();
            }
        } else {
            host_name.push(c.to_ascii_lowercase());
        }
    }
    host_name
}
