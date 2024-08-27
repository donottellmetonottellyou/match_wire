use clap::{CommandFactory, Parser};
use cli::{Cli, Command, Filters, UserInput};
use commands::filter::build_favorites;
use h2m_favorites::*;
use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    runtime::Handle,
    signal,
    sync::{mpsc, Mutex},
    task::JoinHandle,
};
use tracing::{error, info, instrument};
use utils::{
    caching::{build_cache, read_cache, update_cache, Cache},
    json_data::CacheFile,
    subscriber::init_subscriber,
};

fn main() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        error!(name: "PANIC", "{}", format_panic_info(info));
        prev(info);
    }));

    let main_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Failed to create single-threaded runtime");

    let cli = Cli::parse();
    let command_runtime = if cli.single_thread {
        new_io_error!(
            ErrorKind::PermissionDenied,
            "User chose to run on a single thread"
        )
    } else {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
    };

    let command_handle = command_runtime
        .as_ref()
        .map(|rt| rt.handle())
        .unwrap_or_else(|err| {
            if err.kind() != ErrorKind::PermissionDenied {
                error!("{err}");
            }
            main_runtime.handle()
        });

    main_runtime.block_on(async {
        let app_startup = command_handle.spawn(async move {
            app_startup().await
        });

        get_latest_version()
            .await
            .unwrap_or_else(|err| error!("{err}"));

        let (update_cache_tx, mut update_cache_rx) = mpsc::unbounded_channel();
        let cache_needs_update_arc = Arc::new(AtomicBool::new(false));

        tokio::spawn({
            let cache_needs_update = cache_needs_update_arc.clone();
            async move {
                loop {
                    if cache_needs_update.compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst).is_ok()
                        && update_cache_tx.send(true).is_err() {
                            break;
                    }
                    tokio::time::sleep(tokio::time::Duration::from_secs(240)).await;
                }
            }
        });

        let mut stdin = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        let mut stdout = BufWriter::new(tokio::io::stdout());
        let mut shutdown_signal = false;

        let (cache, local_env_dir, exe_dir) = match app_startup.await {
            Ok(startup_result) => match startup_result {
                Ok(data) => data,
                Err(err) => {
                    eprintln!("{err}");
                    await_user_for_end().await;
                    return;
                }
            },
            Err(err) => {
                error!("{err}");
                await_user_for_end().await;
                return
            }
        };

        let exe_dir_arc = Arc::new(exe_dir);
        let local_env_dir_arc = Arc::new(local_env_dir);
        let cache_arc = Arc::new(Mutex::new(cache));

        let command_context = CommandContext {
            cache: &cache_arc,
            exe_dir: &exe_dir_arc,
            local_dir: &local_env_dir_arc,
            cache_needs_update: &cache_needs_update_arc,
            command_runtime: command_handle,
        };

        UserInput::command().print_help().expect("Failed to print help");

        let mut stdout_unchanged = false;

        loop {
            let mut processing_taks = Vec::new();
            if stdout_unchanged {
                stdout_unchanged = false;
            } else {
                print_stdin_ready(&mut stdout).await.unwrap();
            }
            tokio::select! {
                Some(_) = update_cache_rx.recv() => {
                    update_cache(cache_arc.clone(), local_env_dir_arc.clone()).await.unwrap_or_else(|err| error!("{err}"));
                    stdout_unchanged = true;
                    continue;
                }
                _ = signal::ctrl_c() => {
                    shutdown_signal = true;
                    break;
                }
                result = stdin.next_line() => {
                    match result {
                        Ok(None) => continue,
                        Ok(Some(line)) if line.is_empty() => continue,
                        Ok(Some(line)) => {
                            // MARK: TODO
                            // need to lock input while commands are being processed
                            let command_handle = match shellwords::split(&line) {
                                Ok(user_args) => try_execute_command(user_args, &command_context),
                                Err(err) => {
                                    error!("{err}");
                                    continue;
                                }
                            };
                            if command_handle.exit {
                                break;
                            }
                            if let Some(join_handle) = command_handle.handle {
                                processing_taks.push(join_handle);
                            }
                        }
                        Err(err) => {
                            error!("{err}");
                            continue;
                        }
                    }
                }
            }
            for task in processing_taks {
                if let Err(err) = task.await {
                    error!("{err}");
                }
            }
        }

        if cache_needs_update_arc.load(Ordering::SeqCst) {
            update_cache(cache_arc, local_env_dir_arc).await.unwrap_or_else(|err| error!("{err}"));
        }
        if shutdown_signal {
            std::process::exit(0);
        }
    });
}

struct CommandContext<'a> {
    cache: &'a Arc<Mutex<Cache>>,
    exe_dir: &'a Arc<PathBuf>,
    local_dir: &'a Arc<Option<PathBuf>>,
    cache_needs_update: &'a Arc<AtomicBool>,
    command_runtime: &'a Handle,
}

#[derive(Default)]
struct CommandHandle {
    handle: Option<JoinHandle<()>>,
    exit: bool,
}

impl CommandHandle {
    fn exit() -> Self {
        CommandHandle {
            handle: None,
            exit: true,
        }
    }

    fn with_handle(handle: JoinHandle<()>) -> Self {
        CommandHandle {
            handle: Some(handle),
            exit: false,
        }
    }
}

fn try_execute_command(
    mut user_args: Vec<String>,
    command_context: &CommandContext,
) -> CommandHandle {
    let mut input_tokens = vec![String::new()];
    input_tokens.append(&mut user_args);
    match UserInput::try_parse_from(input_tokens) {
        Ok(cli) => match cli.command {
            Command::Filter { args } => new_favorites_with(args, command_context),
            Command::Reconnect {
                history: show_history,
            } => reconnect(show_history),
            Command::GameDir => open_dir(Some(command_context.exe_dir.as_path())),
            Command::LocalEnv => open_dir(command_context.local_dir.as_ref().as_ref()),
            Command::Quit => CommandHandle::exit(),
        },
        Err(err) => {
            if let Err(err) = err.print() {
                error!("{err}");
            }
            CommandHandle::default()
        }
    }
}

fn new_favorites_with(args: Option<Filters>, command_context: &CommandContext) -> CommandHandle {
    let cache = Arc::clone(command_context.cache);
    let exe_dir = Arc::clone(command_context.exe_dir);
    let cache_needs_update = Arc::clone(command_context.cache_needs_update);
    let task_join = command_context.command_runtime.spawn(async move {
        let result = build_favorites(exe_dir, &args.unwrap_or_default(), cache)
            .await
            .unwrap_or_else(|err| {
                error!("{err}");
                false
            });
        if result {
            cache_needs_update.store(true, Ordering::SeqCst);
        }
    });
    CommandHandle::with_handle(task_join)
}

fn reconnect(show_history: bool) -> CommandHandle {
    if show_history {
        todo!();
    }
    todo!()
}

fn open_dir<P: AsRef<Path>>(path: Option<P>) -> CommandHandle {
    if let Some(dir) = path {
        if let Err(err) = std::process::Command::new("explorer")
            .arg(dir.as_ref())
            .spawn()
        {
            error!("{err}")
        };
    } else {
        error!("Could not find local dir");
    }
    CommandHandle::default()
}

async fn print_stdin_ready(buf_writer: &mut BufWriter<tokio::io::Stdout>) -> std::io::Result<()> {
    buf_writer.write_all(b"h2m_favorites.exe> ").await?;
    buf_writer.flush().await
}

pub async fn await_user_for_end() {
    println!("Press enter to exit...");
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let _ = reader.read_line(&mut String::new()).await;
}

#[instrument(skip_all)]
async fn app_startup() -> std::io::Result<(Cache, Option<PathBuf>, PathBuf)> {
    let exe_dir = std::env::current_dir()
        .map_err(|err| std::io::Error::other(format!("Failed to get current dir, {err:?}")))?;

    #[cfg(not(debug_assertions))]
    match does_dir_contain(&exe_dir, Operation::Count, &REQUIRED_FILES)
        .expect("Failed to read contents of current dir")
    {
        OperationResult::Count((count, _)) if count == REQUIRED_FILES.len() => (),
        OperationResult::Count((_, files)) => {
            if !files.contains(REQUIRED_FILES[0]) {
                return new_io_error!(
                    ErrorKind::Other,
                    "Move h2m_favorites.exe into your 'Call of Duty Modern Warfare Remastered' directory"
                );
            } else if !files.contains(REQUIRED_FILES[1]) {
                return new_io_error!(
                    ErrorKind::Other,
                    "H2M mod files not found, h2m_favorites.exe must be placed in 'Call of Duty Modern Warfare Remastered' directory"
                );
            }
            if !files.contains(REQUIRED_FILES[2]) {
                std::fs::create_dir(exe_dir.join(REQUIRED_FILES[2]))
                    .expect("Failed to create players2 folder");
                println!("players2 folder is missing, a new one was created");
            }
        }
        _ => unreachable!(),
    }

    let mut local_env_dir = None;
    if let Some(path) = std::env::var_os(LOCAL_DATA) {
        let mut dir = PathBuf::from(path);

        if let Err(err) = check_app_dir_exists(&mut dir) {
            error!(name: LOG_ONLY, "{err:?}");
        } else {
            init_subscriber(&dir).unwrap_or_else(|err| eprintln!("{err}"));
            info!(name: LOG_ONLY, "App startup");
            local_env_dir = Some(dir);
            match read_cache(local_env_dir.as_ref().unwrap()) {
                Ok(cache) => return Ok((cache, local_env_dir, exe_dir)),
                Err(err) => info!("{err}"),
            }
        }
    } else {
        error!(name: LOG_ONLY, "Could not find %appdata%/local");
        if cfg!(debug_assertions) {
            init_subscriber(Path::new("")).unwrap();
        }
    }
    let server_cache = build_cache().await.map_err(std::io::Error::other)?;
    if let Some(ref dir) = local_env_dir {
        match std::fs::File::create(dir.join(CACHED_DATA)) {
            Ok(file) => {
                let data = CacheFile {
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    created: std::time::SystemTime::now(),
                    cache: server_cache,
                };
                if let Err(err) = serde_json::to_writer_pretty(file, &data) {
                    error!("{err}")
                }
                return Ok((
                    Cache::from(data.cache, data.created),
                    local_env_dir,
                    exe_dir,
                ));
            }
            Err(err) => error!("{err}"),
        }
    }
    Ok((
        Cache::from(server_cache, std::time::SystemTime::now()),
        local_env_dir,
        exe_dir,
    ))
}
