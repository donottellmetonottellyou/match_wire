use clap::Parser;
use cli::Cli;
use commands::filter::{build_favorites, MASTER_URL};
use h2m_favorites::*;
use tracing::error;

#[tokio::main]
async fn main() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        error!(name: "PANIC", "{}", format_panic_info(info));
        prev(info);
    }));

    let (mut cache, local_env_dir) = match app_startup().await {
        Ok(cache) => cache,
        Err(err) => {
            eprintln!("Failed to reach server host {MASTER_URL}, {err:?}");
            await_user_for_end();
            return;
        }
    };

    let exe_dir = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("Failed to get current dir, {err:?}");
            await_user_for_end();
            return;
        }
    };

    #[cfg(not(debug_assertions))]
    match does_dir_contain(&exe_dir, Operation::Count, &REQUIRED_FILES)
        .expect("Failed to read contents of current dir")
    {
        OperationResult::Count((count, _)) if count == REQUIRED_FILES.len() => (),
        OperationResult::Count((_, files)) => {
            if !files.contains(REQUIRED_FILES[0]) {
                eprintln!("Move h2m_favorites.exe into your 'Call of Duty Modern Warfare Remastered' directory");
                await_user_for_end();
                return;
            } else if !files.contains(REQUIRED_FILES[1]) {
                eprintln!("H2M mod files not found, h2m_favorites.exe must be placed in 'Call of Duty Modern Warfare Remastered' directory");
                await_user_for_end();
                return;
            }
            if !files.contains(REQUIRED_FILES[2]) {
                std::fs::create_dir(exe_dir.join(REQUIRED_FILES[2]))
                    .expect("Failed to create players2 folder");
                println!("players2 folder is missing, a new one was created");
            }
        }
        _ => unreachable!(),
    }
    get_latest_version()
        .await
        .unwrap_or_else(|err| error!("{err}"));

    let cli = Cli::parse();
    let cache_needs_update = build_favorites(&exe_dir, &cli, &mut cache)
        .await
        .unwrap_or_else(|err| {
            error!("{err:?}");
            eprintln!("{err:?}");
            false
        });
    if let Some(ref local_dir) = local_env_dir {
        if cli.region.is_some() && cache_needs_update {
            update_cache(cache, local_dir).unwrap_or_else(|err| error!("{err}"));
        }
    }
    await_user_for_end();
}
