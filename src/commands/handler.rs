use crate::{
    cli::{Command, Filters, UserCommand},
    commands::{
        filter::build_favorites,
        launch_h2m::{initalize_listener, launch_h2m_pseudo, HostName},
        reconnect::reconnect,
    },
    utils::caching::{build_cache, Cache},
    CACHED_DATA,
};
use clap::Parser;
use std::{
    fmt::Display,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::{runtime, sync::Mutex, task::JoinHandle};
use tracing::error;
use winptyrs::PTY;

pub struct CommandContext<'a> {
    cache: Arc<Mutex<Cache>>,
    exe_dir: Arc<PathBuf>,
    cache_needs_update: Arc<AtomicBool>,
    command_runtime: &'a runtime::Handle,
    connected_to_pseudoterminal: Arc<AtomicBool>,
    h2m_console_history: Arc<Mutex<Vec<String>>>,
    h2m_server_connection_history: Arc<Mutex<Vec<HostName>>>,
    h2m_handle: Option<Arc<PTY>>,
    command_entered: bool,
    local_dir: Option<Arc<PathBuf>>,
}

impl<'a> CommandContext<'a> {
    pub fn cache(&self) -> Arc<Mutex<Cache>> {
        self.cache.clone()
    }
    pub fn exe_dir(&self) -> Arc<PathBuf> {
        self.exe_dir.clone()
    }
    pub fn cache_needs_update(&self) -> Arc<AtomicBool> {
        self.cache_needs_update.clone()
    }
    pub fn command_runtime(&self) -> &runtime::Handle {
        self.command_runtime
    }
    pub fn check_h2m_connection(&mut self) -> Result<(), &'static str> {
        if !self.connected_to_pseudoterminal.load(Ordering::Relaxed) {
            self.h2m_disconnected();
            return Err("H2M connection closed, restart H2M using the 'launch' command");
        }
        Ok(())
    }
    pub fn connected_to_pseudoterminal(&self) -> Arc<AtomicBool> {
        self.connected_to_pseudoterminal.clone()
    }
    pub fn local_dir(&self) -> Option<Arc<PathBuf>> {
        self.local_dir.as_ref().map(Arc::clone)
    }
    pub fn update_local_dir(&mut self, local_dir: PathBuf) {
        self.local_dir = Some(Arc::new(local_dir))
    }
    pub fn h2m_console_history(&self) -> Arc<Mutex<Vec<String>>> {
        self.h2m_console_history.clone()
    }
    pub fn h2m_server_connection_history(&self) -> Arc<Mutex<Vec<HostName>>> {
        self.h2m_server_connection_history.clone()
    }
    pub fn h2m_handle(&self) -> Option<Arc<PTY>> {
        self.h2m_handle.as_ref().map(Arc::clone)
    }
    pub fn h2m_disconnected(&mut self) {
        self.h2m_handle = None
    }
    /// NOTE: Only intended to be called by `initalize_listener(..)`
    pub fn set_listener(&mut self, handle: PTY) {
        self.connected_to_pseudoterminal
            .store(true, Ordering::SeqCst);
        self.h2m_handle = Some(Arc::new(handle))
    }
    pub fn was_command_entered(&self) -> bool {
        self.command_entered
    }
    fn command_entered(&mut self) {
        self.command_entered = true
    }
    pub fn command_handled(&mut self) {
        self.command_entered = false
    }
}

#[derive(Default)]
pub struct CommandContextBuilder<'a> {
    cache: Option<Arc<Mutex<Cache>>>,
    exe_dir: Option<Arc<PathBuf>>,
    command_runtime: Option<&'a runtime::Handle>,
    local_dir: Option<Arc<PathBuf>>,
    h2m_server_connection_history: Option<Arc<Mutex<Vec<HostName>>>>,
}

impl<'a> CommandContextBuilder<'a> {
    pub fn new() -> Self {
        CommandContextBuilder::default()
    }

    pub fn cache(mut self, cache: Cache) -> Self {
        self.cache = Some(Arc::new(Mutex::new(cache)));
        self
    }
    pub fn exe_dir(mut self, exe_dir: PathBuf) -> Self {
        self.exe_dir = Some(Arc::new(exe_dir));
        self
    }
    pub fn command_runtime(mut self, command_runtime: &'a runtime::Handle) -> Self {
        self.command_runtime = Some(command_runtime);
        self
    }
    pub fn local_dir(mut self, local_dir: Option<PathBuf>) -> Self {
        self.local_dir = local_dir.map(Arc::new);
        self
    }
    pub fn h2m_server_connection_history(
        mut self,
        h2m_server_connection_history: Vec<HostName>,
    ) -> Self {
        self.h2m_server_connection_history =
            Some(Arc::new(Mutex::new(h2m_server_connection_history)));
        self
    }

    pub fn build(self) -> Result<CommandContext<'a>, &'static str> {
        Ok(CommandContext {
            cache: self.cache.ok_or("cache is required")?,
            exe_dir: self.exe_dir.ok_or("exe_dir is required")?,
            command_runtime: self.command_runtime.ok_or("command_runtime is required")?,
            h2m_server_connection_history: self
                .h2m_server_connection_history
                .unwrap_or_else(|| Arc::new(Mutex::new(Vec::new()))),
            cache_needs_update: Arc::new(AtomicBool::new(false)),
            h2m_console_history: Arc::new(Mutex::new(Vec::<String>::new())),
            connected_to_pseudoterminal: Arc::new(AtomicBool::new(false)),
            local_dir: self.local_dir,
            h2m_handle: None,
            command_entered: false,
        })
    }
}

#[derive(Default)]
pub struct CommandHandle {
    pub handle: Option<JoinHandle<()>>,
    pub exit: bool,
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

pub async fn try_execute_command<'a>(
    mut user_args: Vec<String>,
    context: &mut CommandContext<'a>,
) -> CommandHandle {
    let mut input_tokens = vec![String::new()];
    input_tokens.append(&mut user_args);
    context.command_entered();
    match UserCommand::try_parse_from(input_tokens) {
        Ok(cli) => match cli.command {
            Command::Filter { args } => new_favorites_with(args, context),
            Command::Reconnect { args } => reconnect(args, context).await,
            Command::Launch => launch_handler(context),
            Command::UpdateCache => reset_cache(context).await,
            Command::DisplayLogs => h2m_console_history(&context.h2m_console_history()).await,
            Command::GameDir => open_dir(Some(context.exe_dir.as_path())),
            Command::LocalEnv => open_dir(context.local_dir.as_ref().map(|i| i.as_path())),
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

fn new_favorites_with(args: Option<Filters>, context: &CommandContext) -> CommandHandle {
    let cache = context.cache();
    let exe_dir = context.exe_dir();
    let cache_needs_update = context.cache_needs_update();
    let task_join = context.command_runtime.spawn(async move {
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

async fn reset_cache<'a>(context: &CommandContext<'a>) -> CommandHandle {
    let Some(ref local_dir) = context.local_dir else {
        error!("Can not create cache with out a valid save directory");
        return CommandHandle::default();
    };
    let connection_history = context.h2m_server_connection_history();
    let connection_history = connection_history.lock().await;

    let cache_file = match build_cache(Some(&connection_history)).await {
        Ok(data) => data,
        Err(err) => {
            error!("{err}");
            return CommandHandle::default();
        }
    };
    drop(connection_history);

    match std::fs::File::create(local_dir.join(CACHED_DATA)) {
        Ok(file) => {
            if let Err(err) = serde_json::to_writer_pretty(file, &cache_file) {
                error!("{err}")
            }
        }
        Err(err) => error!("{err}"),
    }
    let cache = context.cache();
    let mut cache = cache.lock().await;
    *cache = Cache::from(cache_file.cache, cache_file.created);
    CommandHandle::default()
}

fn launch_handler(context: &mut CommandContext) -> CommandHandle {
    match launch_h2m_pseudo(&context.exe_dir) {
        Ok(handle) => initalize_listener(handle, context),
        Err(err) => error!("{err}"),
    }
    CommandHandle::default()
}

struct DisplayLogs<'a>(&'a [String]);

impl<'a> Display for DisplayLogs<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for line in self.0 {
            writeln!(f, "{line}")?;
        }
        Ok(())
    }
}

async fn h2m_console_history(history: &Mutex<Vec<String>>) -> CommandHandle {
    let history = history.lock().await;
    println!("{}", DisplayLogs(&history));
    CommandHandle::default()
}

fn open_dir(path: Option<&Path>) -> CommandHandle {
    if let Some(dir) = path {
        if let Err(err) = std::process::Command::new("explorer").arg(dir).spawn() {
            error!("{err}")
        };
    } else {
        error!("Could not find local dir");
    }
    CommandHandle::default()
}
