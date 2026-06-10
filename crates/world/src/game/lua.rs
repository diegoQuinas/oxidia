//! Lua scripting runtime for the game actor.
//!
//! Wraps `mlua::Lua` in a safe public API behind `#![forbid(unsafe_code)]`.
//! The runtime is `Option<LuaRuntime>` on `Game` — absent when initialisation
//! fails or no scripts directory exists.
//!
//! ## Safety
//!
//! `mlua::Lua` is `!Send`, but `Game` runs on a single-threaded actor task, so
//! the runtime never crosses an `.await` boundary.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use mlua::{Lua, Value};
use thiserror::Error;

use crate::Position;

/// Actions a Lua script can request. Queued during dispatch, drained by the
/// caller (e.g. `do_use_item`) which holds `&mut Game` and can execute them.
#[derive(Debug, Clone, Copy)]
pub(crate) enum GameAction {
    Teleport { player_id: u32, landing: Position },
}

/// Arguments dispatched to a Lua callback, passed as a Lua table.
///
/// `dead_code` suppressed until PR 2 (Phase 3) wires dispatch into do_use_item.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct LuaArgs {
    pub player_id: u32,
    pub item_id: u16,
    pub pos_x: u16,
    pub pos_y: u16,
    pub pos_z: u8,
    pub stackpos: u8,
}

/// Errors originating from the Lua runtime.
#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum LuaError {
    /// Wraps an `mlua::Error` (script error, type mismatch, etc.).
    #[error("Lua runtime error: {0}")]
    Runtime(#[from] mlua::Error),
}

/// Safe wrapper around `mlua::Lua` for the game actor.
///
/// Lives as `Option<LuaRuntime>` on `Game` — the actor starts without a runtime
/// if no scripts directory exists or initialisation fails.
///
/// `dead_code` suppressed until PR 2 (Phase 3) wires dispatch into do_use_item.
#[allow(dead_code)]
pub struct LuaRuntime {
    lua: Lua,
    scripts_dir: PathBuf,
    /// Queue of actions requested by Lua scripts during dispatch.
    /// `Arc<Mutex<>>` is needed because `mlua` closures require `Send` on this
    /// platform and the `Game` actor runs in a `tokio::spawn` (must be `Send`).
    actions: Arc<Mutex<Vec<GameAction>>>,
}

#[allow(dead_code)]
impl LuaRuntime {
    /// Create a new runtime and load all `.lua` files from `scripts_dir`.
    ///
    /// Errors during script loading are logged via `tracing::error` but do not
    /// block startup — the runtime starts with whatever scripts loaded
    /// successfully.
    pub fn new(scripts_dir: &Path) -> Self {
        let lua = Lua::new();
        let actions: Arc<Mutex<Vec<GameAction>>> = Arc::new(Mutex::new(Vec::new()));
        Self::bind_builtins(&lua, Arc::clone(&actions));
        let rt = Self { lua, scripts_dir: scripts_dir.to_path_buf(), actions };
        rt.load_scripts();
        rt
    }

    /// Drop the current Lua state and reload all scripts from disk.
    ///
    /// Safe to call mid-game: the old state is dropped, a fresh one is created,
    /// and the same `scripts_dir` is re-scanned.
    pub fn reload(&mut self) {
        self.lua = Lua::new();
        // Create a fresh actions queue so stale actions from the old state are dropped.
        self.actions = Arc::new(Mutex::new(Vec::new()));
        Self::bind_builtins(&self.lua, Arc::clone(&self.actions));
        self.load_scripts();
    }

    /// Dispatch a Lua callback by name.
    ///
    /// Looks up `fn_name` in the global Lua table, converts `args` to a Lua
    /// table, and calls the function via protected call.
    ///
    /// Returns:
    /// - `Ok(true)` — function found and returned a truthy value
    /// - `Ok(false)` — function not found, or returned `false` / `nil`
    /// - `Err(LuaError)` — Lua error during execution (caught by pcall)
    pub fn dispatch(&self, fn_name: &str, args: &LuaArgs) -> Result<bool, LuaError> {
        let globals = self.lua.globals();
        let func: Value = globals.get(fn_name)?;
        match func {
            Value::Function(f) => {
                let table = self.lua.create_table()?;
                table.set("player_id", args.player_id)?;
                table.set("item_id", args.item_id)?;
                table.set("pos_x", args.pos_x)?;
                table.set("pos_y", args.pos_y)?;
                table.set("pos_z", args.pos_z)?;
                table.set("stackpos", args.stackpos)?;
                let result: Value = f.call(table)?;
                match result {
                    Value::Boolean(b) => Ok(b),
                    Value::Nil => Ok(false),
                    _ => Ok(true),
                }
            }
            _ => Ok(false),
        }
    }

    /// Register built-in Rust functions in the Lua global table so scripts can
    /// request game actions (e.g. teleporting a player). Each closure captures a
    /// clone of `actions` via `Rc` and pushes a [`GameAction`] on invocation.
    ///
    /// Called from both `new` and `reload` so every fresh Lua state gets bindings.
    fn bind_builtins(lua: &Lua, actions: Arc<Mutex<Vec<GameAction>>>) {
        let do_teleport = lua
            .create_function(move |_, (id, x, y, z): (u32, u16, u16, u8)| {
                actions.lock().unwrap().push(GameAction::Teleport {
                    player_id: id,
                    landing: Position::new(x, y, z),
                });
                Ok(())
            })
            .expect("create_function for do_teleport must not fail");
        lua.globals().set("do_teleport", do_teleport)
            .expect("set do_teleport global must not fail");
    }

    /// Drain all queued actions since the last call. Called by the game actor
    /// after `dispatch` returns, while it still holds `&mut self` (Game) so it
    /// can execute each action.
    pub(crate) fn drain_actions(&self) -> Vec<GameAction> {
        self.actions.lock().unwrap().drain(..).collect()
    }

    /// Load every `.lua` file in `scripts_dir` into the Lua state.
    ///
    /// Syntax errors are logged and skipped — a single bad script does not
    /// prevent others from loading.
    fn load_scripts(&self) {
        if !self.scripts_dir.is_dir() {
            return;
        }
        let Ok(entries) = std::fs::read_dir(&self.scripts_dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "lua") {
                match std::fs::read_to_string(&path) {
                    Ok(code) => {
                        if let Err(e) = self.lua.load(&code).exec() {
                            tracing::error!(path = %path.display(), error = %e, "failed to load Lua script");
                        }
                    }
                    Err(e) => {
                        tracing::error!(path = %path.display(), error = %e, "failed to read Lua script");
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Test helpers (behind #[cfg(test)] to keep them out of release builds)
// ---------------------------------------------------------------------------

#[cfg(test)]
impl LuaRuntime {
    /// Read an `i64` global from the Lua state. Used by integration tests to
    /// verify that a Lua callback was actually dispatched.
    pub fn get_global_i64(&self, name: &str) -> Option<i64> {
        self.lua.globals().get(name).ok()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU16, Ordering};

    use super::{LuaArgs, LuaRuntime};

    /// Create a fresh temp directory for a test case.
    fn test_dir(label: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU16 = AtomicU16::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("oxidia-lua-{label}-{seq}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_args() -> LuaArgs {
        LuaArgs { player_id: 1, item_id: 1386, pos_x: 100, pos_y: 100, pos_z: 7, stackpos: 0 }
    }

    // ------------------------------------------------------------------
    // RED 1: new loads scripts from dir → dispatch works
    // ------------------------------------------------------------------

    #[test]
    fn new_loads_scripts_and_dispatch_calls_function() {
        let dir = test_dir("load");
        fs::write(dir.join("test.lua"), b"function onUse(args) return true end").unwrap();
        let rt = LuaRuntime::new(&dir);
        let result = rt.dispatch("onUse", &sample_args()).unwrap();
        assert!(result, "dispatch of registered onUse must return true");
        let _ = fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------------------
    // RED 2: dispatch returns false for missing function
    // ------------------------------------------------------------------

    #[test]
    fn dispatch_returns_false_for_missing_function() {
        let dir = test_dir("missing_fn");
        fs::write(dir.join("test.lua"), b"function onUse(args) return true end").unwrap();
        let rt = LuaRuntime::new(&dir);
        let result = rt.dispatch("nonexistent", &sample_args()).unwrap();
        assert!(!result, "dispatch of missing function must return Ok(false)");
        let _ = fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------------------
    // RED 3: reload refreshes Lua state
    // ------------------------------------------------------------------

    #[test]
    fn reload_reflects_new_script_content() {
        let dir = test_dir("reload");
        fs::write(dir.join("test.lua"), b"function onUse(args) return false end").unwrap();
        let mut rt = LuaRuntime::new(&dir);
        // First version: returns false
        assert!(!rt.dispatch("onUse", &sample_args()).unwrap());

        // Replace script with new implementation
        fs::write(dir.join("test.lua"), b"function onUse(args) return true end").unwrap();
        rt.reload();
        assert!(rt.dispatch("onUse", &sample_args()).unwrap(), "after reload new script must be active");
        let _ = fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------------------
    // RED 4: missing scripts dir → runtime starts empty
    // ------------------------------------------------------------------

    #[test]
    fn missing_scripts_dir_starts_empty() {
        let dir = test_dir("empty");
        // Do NOT create the dir — start with a non-existent path
        let phantom = dir.join("nonexistent");
        let rt = LuaRuntime::new(&phantom);
        let result = rt.dispatch("onUse", &sample_args()).unwrap();
        assert!(!result, "runtime without scripts must return Ok(false)");
        // Cleanup: remove the base test dir
        let _ = fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------------------
    // TRIANGULATE: callback can return false explicitly
    // ------------------------------------------------------------------

    #[test]
    fn dispatch_returns_false_when_callback_returns_false() {
        let dir = test_dir("returns_false");
        fs::write(dir.join("test.lua"), b"function onUse(args) return false end").unwrap();
        let rt = LuaRuntime::new(&dir);
        let result = rt.dispatch("onUse", &sample_args()).unwrap();
        assert!(!result, "callback returning false must yield Ok(false)");
        let _ = fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------------------
    // TRIANGULATE: callback that returns non-boolean is truthy
    // ------------------------------------------------------------------

    #[test]
    fn dispatch_truthy_for_non_boolean_return() {
        let dir = test_dir("truthy");
        fs::write(dir.join("test.lua"), b"function onUse(args) return 42 end").unwrap();
        let rt = LuaRuntime::new(&dir);
        let result = rt.dispatch("onUse", &sample_args()).unwrap();
        assert!(result, "callback returning number must yield Ok(true)");
        let _ = fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------------------
    // TRIANGULATE: function calls use correct args table
    // ------------------------------------------------------------------

    #[test]
    fn dispatch_passes_args_as_table() {
        let dir = test_dir("args_table");
        fs::write(
            dir.join("test.lua"),
            br#"
            function onUse(args)
                return args.player_id == 1 and args.item_id == 1386
            end
            "#,
        ).unwrap();
        let rt = LuaRuntime::new(&dir);
        let result = rt.dispatch("onUse", &sample_args()).unwrap();
        assert!(result, "callback that validates args must succeed");
        let _ = fs::remove_dir_all(&dir);
    }
}
