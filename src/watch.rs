//! File-system events for the checkouts the panes show, so they run git when
//! something changed instead of on every tick (FSEvents on macOS, inotify on
//! Linux, through `notify`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use crate::changes::Event;

/// Paths whose changes never change a diff: git's object store and logs,
/// and the build and dependency trees agents churn through.
const IGNORED: [&str; 7] = [
    "/.git/objects/",
    "/.git/logs/",
    "/target/",
    "/node_modules/",
    "/build/",
    "/.gradle/",
    "/dist/",
];

/// Checkouts being watched, and which have changed since last asked.
pub struct Watch {
    watcher: RecommendedWatcher,
    roots: Arc<Mutex<Vec<PathBuf>>>,
    dirty: Arc<Mutex<HashSet<PathBuf>>>,
}

impl Watch {
    /// A watcher that wakes the pane through `wake` when a watched checkout
    /// first changes; `None` where the platform offers no events.
    pub fn new(wake: Sender<Event>) -> Option<Self> {
        let roots: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
        let dirty: Arc<Mutex<HashSet<PathBuf>>> = Arc::default();
        let (seen_roots, seen_dirty) = (roots.clone(), dirty.clone());
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else {
                return;
            };
            let roots = seen_roots.lock().unwrap_or_else(|e| e.into_inner());
            let mut dirty = seen_dirty.lock().unwrap_or_else(|e| e.into_inner());
            for path in &event.paths {
                let shown = path.to_string_lossy();
                if IGNORED.iter().any(|i| shown.contains(i)) {
                    continue;
                }
                // Wake once per checkout until the pane has looked.
                if let Some(root) = roots.iter().find(|r| path.starts_with(r))
                    && dirty.insert(root.clone())
                {
                    let _ = wake.send(Event::Wake);
                }
            }
        })
        .ok()?;
        Some(Self {
            watcher,
            roots,
            dirty,
        })
    }

    /// Watches `root` from now on, and counts it as changed so it is read
    /// once.
    pub fn add(&mut self, root: &str) {
        let path = Path::new(root)
            .canonicalize()
            .unwrap_or_else(|_| root.into());
        let mut roots = self.roots.lock().unwrap_or_else(|e| e.into_inner());
        if roots.contains(&path) {
            return;
        }
        if self.watcher.watch(&path, RecursiveMode::Recursive).is_ok() {
            roots.push(path.clone());
            self.dirty
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(path);
        }
    }

    /// Whether `root` changed since last asked; asking clears it. A root not
    /// watched always counts as changed.
    pub fn take(&self, root: &str) -> bool {
        let path = Path::new(root)
            .canonicalize()
            .unwrap_or_else(|_| root.into());
        let watched = self
            .roots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&path);
        !watched
            || self
                .dirty
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&path)
    }
}
