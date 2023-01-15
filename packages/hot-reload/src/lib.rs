use std::{
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    str::FromStr,
    sync::{Arc, Mutex},
};

use dioxus_core::Template;
use dioxus_rsx::{
    hot_reload::{FileMap, UpdateResult},
    HotReloadingContext,
};
use interprocess::local_socket::{LocalSocketListener, LocalSocketStream};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

#[cfg(debug_assertions)]
pub use dioxus_html::HtmlCtx;
use serde::{Deserialize, Serialize};

/// A message the hot reloading server sends to the client
#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
pub enum HotReloadMsg {
    /// A template has been updated
    #[serde(borrow = "'static")]
    UpdateTemplate(Template<'static>),
    /// The program needs to be recompiled, and the client should shut down
    Shutdown,
}

pub struct Config<Ctx: HotReloadingContext = HtmlCtx> {
    root_path: &'static str,
    listening_paths: &'static [&'static str],
    excluded_paths: &'static [&'static str],
    log: bool,
    rebuild_with: Option<Box<dyn FnMut() -> bool + Send + 'static>>,
    phantom: std::marker::PhantomData<Ctx>,
}

impl<Ctx: HotReloadingContext> Default for Config<Ctx> {
    fn default() -> Self {
        Self {
            root_path: "",
            listening_paths: &[""],
            excluded_paths: &["./target"],
            log: true,
            rebuild_with: None,
            phantom: std::marker::PhantomData,
        }
    }
}

impl Config<HtmlCtx> {
    pub const fn new() -> Self {
        Self {
            root_path: "",
            listening_paths: &[""],
            excluded_paths: &["./target"],
            log: true,
            rebuild_with: None,
            phantom: std::marker::PhantomData,
        }
    }
}

impl<Ctx: HotReloadingContext> Config<Ctx> {
    /// Set the root path of the project (where the Cargo.toml file is). This is automatically set by the [`hot_reload_init`] macro.
    pub fn root(self, path: &'static str) -> Self {
        Self {
            root_path: path,
            ..self
        }
    }

    /// Set whether to enable logs
    pub fn with_logging(self, log: bool) -> Self {
        Self { log, ..self }
    }

    /// Set the command to run to rebuild the project
    ///
    /// For example to restart the application after a change is made, you could use `cargo run`
    pub fn with_rebuild_command(self, rebuild_command: &'static str) -> Self {
        self.with_rebuild_callback(move || {
            execute::shell(rebuild_command)
                .spawn()
                .expect("Failed to spawn the rebuild command");
            true
        })
    }

    /// Set a callback to run to when the project needs to be rebuilt and returns if the server should shut down
    ///
    /// For example a CLI application could rebuild the application when a change is made
    pub fn with_rebuild_callback(
        self,
        rebuild_callback: impl FnMut() -> bool + Send + 'static,
    ) -> Self {
        Self {
            rebuild_with: Some(Box::new(rebuild_callback)),
            ..self
        }
    }

    /// Set the paths to listen for changes in to trigger hot reloading. If this is a directory it will listen for changes in all files in that directory recursively.
    pub fn with_paths(self, paths: &'static [&'static str]) -> Self {
        Self {
            listening_paths: paths,
            ..self
        }
    }

    /// Sets paths to ignore changes on. This will override any paths set in the [`Config::with_paths`] method in the case of conflicts.
    pub fn excluded_paths(self, paths: &'static [&'static str]) -> Self {
        Self {
            excluded_paths: paths,
            ..self
        }
    }
}

/// Initialize the hot reloading listener
pub fn init<Ctx: HotReloadingContext + Send + 'static>(cfg: Config<Ctx>) {
    let Config {
        root_path,
        listening_paths,
        log,
        mut rebuild_with,
        excluded_paths,
        phantom: _,
    } = cfg;

    if let Ok(crate_dir) = PathBuf::from_str(root_path) {
        let temp_file = std::env::temp_dir().join("@dioxusin");
        let channels = Arc::new(Mutex::new(Vec::new()));
        let file_map = Arc::new(Mutex::new(FileMap::<Ctx>::new(crate_dir.clone())));
        if let Ok(local_socket_stream) = LocalSocketListener::bind(temp_file.as_path()) {
            let aborted = Arc::new(Mutex::new(false));

            // listen for connections
            std::thread::spawn({
                let file_map = file_map.clone();
                let channels = channels.clone();
                let aborted = aborted.clone();
                let _ = local_socket_stream.set_nonblocking(true);
                move || {
                    loop {
                        if let Ok(mut connection) = local_socket_stream.accept() {
                            // send any templates than have changed before the socket connected
                            let templates: Vec<_> = {
                                file_map
                                    .lock()
                                    .unwrap()
                                    .map
                                    .values()
                                    .filter_map(|(_, template_slot)| *template_slot)
                                    .collect()
                            };
                            for template in templates {
                                if !send_msg(
                                    HotReloadMsg::UpdateTemplate(template),
                                    &mut connection,
                                ) {
                                    continue;
                                }
                            }
                            channels.lock().unwrap().push(connection);
                            if log {
                                println!("Connected to hot reloading 🚀");
                            }
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        if *aborted.lock().unwrap() {
                            break;
                        }
                    }
                }
            });

            // watch for changes
            std::thread::spawn(move || {
                // try to find the gitingore file
                let gitignore_file_path = crate_dir.join(".gitignore");
                let (gitignore, _) = ignore::gitignore::Gitignore::new(gitignore_file_path);

                let mut last_update_time = chrono::Local::now().timestamp();

                let (tx, rx) = std::sync::mpsc::channel();

                let mut watcher = RecommendedWatcher::new(tx, notify::Config::default()).unwrap();

                for path in listening_paths {
                    let full_path = crate_dir.join(path);
                    if let Err(err) = watcher.watch(&full_path, RecursiveMode::Recursive) {
                        if log {
                            println!(
                                "hot reloading failed to start watching {full_path:?}:\n{err:?}",
                            );
                        }
                    }
                }

                let excluded_paths = excluded_paths
                    .iter()
                    .map(|path| crate_dir.join(PathBuf::from(path)))
                    .collect::<Vec<_>>();

                let mut rebuild = {
                    let aborted = aborted.clone();
                    let channels = channels.clone();
                    move || {
                        if let Some(rebuild_callback) = &mut rebuild_with {
                            if log {
                                println!("Rebuilding the application...");
                            }
                            let shutdown = rebuild_callback();

                            if shutdown {
                                *aborted.lock().unwrap() = true;
                            }

                            for channel in &mut *channels.lock().unwrap() {
                                send_msg(HotReloadMsg::Shutdown, channel);
                            }

                            return shutdown;
                        } else if log {
                            println!(
                                "Rebuild needed... shutting down hot reloading.\nManually rebuild the application to view futher changes."
                            );
                        }
                        true
                    }
                };

                for evt in rx {
                    if chrono::Local::now().timestamp() > last_update_time {
                        if let Ok(evt) = evt {
                            let real_paths = evt
                                .paths
                                .iter()
                                .filter(|path| {
                                    // skip non rust files
                                    matches!(
                                        path.extension().and_then(|p| p.to_str()),
                                        Some("rs" | "toml" | "css" | "html" | "js")
                                    )&&
                                    // skip excluded paths
                                    !excluded_paths.iter().any(|p| path.starts_with(p)) &&
                                    // respect .gitignore
                                    !gitignore
                                        .matched_path_or_any_parents(path, false)
                                        .is_ignore()
                                })
                                .collect::<Vec<_>>();

                            // Give time for the change to take effect before reading the file
                            if !real_paths.is_empty() {
                                std::thread::sleep(std::time::Duration::from_millis(10));
                            }

                            let mut channels = channels.lock().unwrap();
                            for path in real_paths {
                                // if this file type cannot be hot reloaded, rebuild the application
                                if path.extension().and_then(|p| p.to_str()) != Some("rs")
                                    && rebuild()
                                {
                                    return;
                                }
                                // find changes to the rsx in the file
                                match file_map
                                    .lock()
                                    .unwrap()
                                    .update_rsx(path, crate_dir.as_path())
                                {
                                    UpdateResult::UpdatedRsx(msgs) => {
                                        for msg in msgs {
                                            let mut i = 0;
                                            while i < channels.len() {
                                                let channel = &mut channels[i];
                                                if send_msg(
                                                    HotReloadMsg::UpdateTemplate(msg),
                                                    channel,
                                                ) {
                                                    i += 1;
                                                } else {
                                                    channels.remove(i);
                                                }
                                            }
                                        }
                                    }
                                    UpdateResult::NeedsRebuild => {
                                        drop(channels);
                                        if rebuild() {
                                            return;
                                        }
                                        break;
                                    }
                                }
                            }
                        }
                        last_update_time = chrono::Local::now().timestamp();
                    }
                }
            });
        }
    }
}

fn send_msg(msg: HotReloadMsg, channel: &mut impl Write) -> bool {
    if let Ok(msg) = serde_json::to_string(&msg) {
        if channel.write_all(msg.as_bytes()).is_err() {
            return false;
        }
        if channel.write_all(&[b'\n']).is_err() {
            return false;
        }
        true
    } else {
        false
    }
}

/// Connect to the hot reloading listener. The callback provided will be called every time a template change is detected
pub fn connect(mut f: impl FnMut(HotReloadMsg) + Send + 'static) {
    std::thread::spawn(move || {
        let temp_file = std::env::temp_dir().join("@dioxusin");
        if let Ok(socket) = LocalSocketStream::connect(temp_file.as_path()) {
            let mut buf_reader = BufReader::new(socket);
            loop {
                let mut buf = String::new();
                match buf_reader.read_line(&mut buf) {
                    Ok(_) => {
                        let template: HotReloadMsg =
                            serde_json::from_str(Box::leak(buf.into_boxed_str())).unwrap();
                        f(template);
                    }
                    Err(err) => {
                        if err.kind() != std::io::ErrorKind::WouldBlock {
                            break;
                        }
                    }
                }
            }
        }
    });
}

/// Start the hot reloading server with the current directory as the root
#[macro_export]
macro_rules! hot_reload_init {
    () => {
        #[cfg(debug_assertions)]
        dioxus_hot_reload::init(dioxus_hot_reload::Config::new().root(env!("CARGO_MANIFEST_DIR")));
    };

    ($cfg: expr) => {
        #[cfg(debug_assertions)]
        dioxus_hot_reload::init($cfg.root(env!("CARGO_MANIFEST_DIR")));
    };
}
