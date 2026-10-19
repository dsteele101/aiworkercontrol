//! The AI Worker Control web app: a dashboard of the worker pool and the
//! settings page, served on the loopback interface only.
//!
//! Pages are rendered on the server, and the dashboard refreshes its live
//! sections every two seconds. The history page is the exception: it draws
//! its filters and charts in the browser from `/history.json`, with a
//! charting library compiled into the binary so it works offline.
//!
//! ```
//! let root = tempfile::tempdir()?;
//! std::fs::create_dir_all(root.path().join("work/open"))?;
//!
//! let state = server::AppState::new(root.path(), 8787)?;
//! assert!(!state.pool().snapshot().running);
//! let app: axum::Router = server::router(state);
//! # drop(app);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod guard;
mod login;
mod pages;
mod routes;
mod settings;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use axum::Router;
use work_queue::Queue;
use worker_pool::Supervisor;

pub use settings::{ProviderSettings, SETTINGS_FILE, Settings};

/// An error starting the app.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// Reading or writing a file failed.
    #[error("{}: {source}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The settings file is not valid TOML for [`Settings`].
    #[error("{}: {source}", path.display())]
    Parse {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: toml::de::Error,
    },
    /// The settings could not be written as TOML.
    #[error("could not serialize settings: {0}")]
    Serialize(#[source] toml::ser::Error),
    /// The worker pool could not be created.
    #[error(transparent)]
    Pool(#[from] worker_pool::PoolError),
}

/// Shared state of the app. Cheap to clone.
#[derive(Debug, Clone)]
pub struct AppState(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    root: PathBuf,
    port: u16,
    settings: Mutex<Settings>,
    pool: Supervisor,
    logins: login::Logins,
    notice: Mutex<Option<String>>,
}

impl AppState {
    /// Loads the settings in `root` and creates the (stopped) worker pool over
    /// `root/work`. `port` is the port the app is served on, which requests
    /// are checked against.
    pub fn new(root: impl Into<PathBuf>, port: u16) -> Result<Self, ServerError> {
        let root = root.into();
        let settings = Settings::load(&root)?;
        let pool = Supervisor::new(Queue::new(root.join("work")), settings.pool_config(&root))?;
        Ok(Self(Arc::new(Inner {
            root,
            port,
            settings: Mutex::new(settings),
            pool,
            logins: login::Logins::default(),
            notice: Mutex::new(None),
        })))
    }

    /// The worker pool.
    pub fn pool(&self) -> &Supervisor {
        &self.0.pool
    }

    /// The directory holding `work/`, `spec/` and the settings file.
    pub fn root(&self) -> &Path {
        &self.0.root
    }

    /// A copy of the current settings.
    pub fn settings(&self) -> Settings {
        lock(&self.0.settings).clone()
    }

    /// Applies `edit` to the settings, then saves them and hands the new
    /// configuration to the pool. An edit returning an error, or one that
    /// leaves the active provider unrunnable, changes nothing.
    pub(crate) fn update_settings(
        &self,
        edit: impl FnOnce(&mut Settings) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut current = lock(&self.0.settings);
        let mut next = current.clone();
        edit(&mut next)?;
        next.active_config().map_err(|e| e.to_string())?;
        next.save(&self.0.root).map_err(|e| e.to_string())?;
        self.0.pool.set_config(next.pool_config(&self.0.root));
        *current = next;
        Ok(())
    }

    pub(crate) fn logins(&self) -> &login::Logins {
        &self.0.logins
    }

    /// Sets the message shown at the top of the next page.
    pub(crate) fn notify(&self, message: impl Into<String>) {
        *lock(&self.0.notice) = Some(message.into());
    }

    /// Takes the pending message, if any.
    pub(crate) fn take_notice(&self) -> Option<String> {
        lock(&self.0.notice).take()
    }
}

/// The app's routes, behind the request guard.
pub fn router(state: AppState) -> Router {
    let port = state.0.port;
    routes::routes()
        .layer(axum::middleware::from_fn(move |req, next| {
            guard::check(port, req, next)
        }))
        .with_state(state)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Every critical section leaves the value consistent, so a panic in one
    // leaves nothing half-written to guard against.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
