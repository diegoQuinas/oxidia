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
#[derive(Debug, Clone)]
pub(crate) enum GameAction {
    Teleport {
        player_id: u32,
        landing: Position,
    },
    Feed {
        player_id: u32,
        health_gain: i32,
        interval_ms: u64,
        duration_ms: u64,
        total_heal_cap: i32,
    },
    TextMessage {
        player_id: u32,
        message_type: u8,
        text: String,
    },
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
        let rt = Self {
            lua,
            scripts_dir: scripts_dir.to_path_buf(),
            actions,
        };
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
                let table = self.build_args_table(args)?;
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

    /// Dispatch to a namespaced Lua function (e.g. `"food.onUse"`).
    ///
    /// Parses `script_name` as `"table.function"`, looks up the table in globals
    /// and calls the function on it. Falls back to [`dispatch`] for flat names.
    ///
    /// This is the primary dispatch path for registered item scripts, ensuring
    /// multiple scripts (food.lua, teleport.lua, etc.) can coexist without
    /// clobbering each other's `onUse` global.
    pub fn dispatch_namespaced(&self, script_name: &str, args: &LuaArgs) -> Result<bool, LuaError> {
        let (table_name, method) = match script_name.split_once('.') {
            Some(pair) => pair,
            None => return self.dispatch(script_name, args),
        };
        let globals = self.lua.globals();
        let table_val: Value = globals.get(table_name)?;
        match table_val {
            Value::Table(t) => {
                let func: Value = t.get(method)?;
                match func {
                    Value::Function(f) => {
                        let table = self.build_args_table(args)?;
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
            _ => Ok(false),
        }
    }

    /// Build an args table for dispatching. Shared by [`dispatch`] and
    /// [`dispatch_namespaced`].
    fn build_args_table(&self, args: &LuaArgs) -> Result<mlua::Table, LuaError> {
        let table = self.lua.create_table()?;
        table.set("player_id", args.player_id)?;
        table.set("item_id", args.item_id)?;
        table.set("pos_x", args.pos_x)?;
        table.set("pos_y", args.pos_y)?;
        table.set("pos_z", args.pos_z)?;
        table.set("stackpos", args.stackpos)?;
        Ok(table)
    }

    /// Register built-in Rust functions in the Lua global table so scripts can
    /// request game actions (e.g. teleporting a player). Each closure captures a
    /// clone of `actions` via `Rc` and pushes a [`GameAction`] on invocation.
    ///
    /// Called from both `new` and `reload` so every fresh Lua state gets bindings.
    fn bind_builtins(lua: &Lua, actions: Arc<Mutex<Vec<GameAction>>>) {
        let teleport_actions = Arc::clone(&actions);
        let do_teleport = lua
            .create_function(move |_, (id, x, y, z): (u32, u16, u16, u8)| {
                teleport_actions.lock().unwrap().push(GameAction::Teleport {
                    player_id: id,
                    landing: Position::new(x, y, z),
                });
                Ok(())
            })
            .expect("create_function for do_teleport must not fail");
        lua.globals()
            .set("do_teleport", do_teleport)
            .expect("set do_teleport global must not fail");

        let feed_actions = Arc::clone(&actions);
        let do_feed = lua
            .create_function(
                move |_,
                      (player_id, health_gain, interval_ms, duration_ms, total_heal_cap): (
                    u32,
                    i32,
                    u64,
                    u64,
                    i32,
                )| {
                    feed_actions.lock().unwrap().push(GameAction::Feed {
                        player_id,
                        health_gain,
                        interval_ms,
                        duration_ms,
                        total_heal_cap,
                    });
                    Ok(())
                },
            )
            .expect("create_function for do_feed must not fail");
        lua.globals()
            .set("do_feed", do_feed)
            .expect("set do_feed global must not fail");

        let text_actions = Arc::clone(&actions);
        let do_send_text_message = lua
            .create_function(move |_, (id, msg_type, text): (u32, u8, String)| {
                text_actions.lock().unwrap().push(GameAction::TextMessage {
                    player_id: id,
                    message_type: msg_type,
                    text,
                });
                Ok(())
            })
            .expect("create_function for do_send_text_message must not fail");
        lua.globals()
            .set("do_send_text_message", do_send_text_message)
            .expect("set do_send_text_message global must not fail");
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
        let Ok(entries) = std::fs::read_dir(&self.scripts_dir) else {
            return;
        };
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

    use super::{GameAction, LuaArgs, LuaRuntime};

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
        LuaArgs {
            player_id: 1,
            item_id: 1386,
            pos_x: 100,
            pos_y: 100,
            pos_z: 7,
            stackpos: 0,
        }
    }

    // ------------------------------------------------------------------
    // RED 1: new loads scripts from dir → dispatch works
    // ------------------------------------------------------------------

    #[test]
    fn new_loads_scripts_and_dispatch_calls_function() {
        let dir = test_dir("load");
        fs::write(
            dir.join("test.lua"),
            b"function onUse(args) return true end",
        )
        .unwrap();
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
        fs::write(
            dir.join("test.lua"),
            b"function onUse(args) return true end",
        )
        .unwrap();
        let rt = LuaRuntime::new(&dir);
        let result = rt.dispatch("nonexistent", &sample_args()).unwrap();
        assert!(
            !result,
            "dispatch of missing function must return Ok(false)"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------------------
    // RED 3: reload refreshes Lua state
    // ------------------------------------------------------------------

    #[test]
    fn reload_reflects_new_script_content() {
        let dir = test_dir("reload");
        fs::write(
            dir.join("test.lua"),
            b"function onUse(args) return false end",
        )
        .unwrap();
        let mut rt = LuaRuntime::new(&dir);
        // First version: returns false
        assert!(!rt.dispatch("onUse", &sample_args()).unwrap());

        // Replace script with new implementation
        fs::write(
            dir.join("test.lua"),
            b"function onUse(args) return true end",
        )
        .unwrap();
        rt.reload();
        assert!(
            rt.dispatch("onUse", &sample_args()).unwrap(),
            "after reload new script must be active"
        );
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
        fs::write(
            dir.join("test.lua"),
            b"function onUse(args) return false end",
        )
        .unwrap();
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
        )
        .unwrap();
        let rt = LuaRuntime::new(&dir);
        let result = rt.dispatch("onUse", &sample_args()).unwrap();
        assert!(result, "callback that validates args must succeed");
        let _ = fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------------------
    // PHASE 2: do_feed binding pushes GameAction::Feed
    // ------------------------------------------------------------------

    #[test]
    fn do_feed_pushes_feed_action_with_correct_params() {
        // RED: do_feed(pid, hg, interval, dur, cap) must push a GameAction::Feed
        // with the same fields.
        let dir = test_dir("do_feed");
        fs::write(
            dir.join("test.lua"),
            br#"
            function onUse(args)
                do_feed(args.player_id, 8, 2000, 60000, 240)
                return true
            end
            "#,
        )
        .unwrap();
        let rt = LuaRuntime::new(&dir);
        rt.dispatch("onUse", &sample_args()).unwrap();
        let actions = rt.drain_actions();
        assert_eq!(actions.len(), 1, "exactly one action must be queued");
        match &actions[0] {
            GameAction::Feed {
                player_id,
                health_gain,
                interval_ms,
                duration_ms,
                total_heal_cap,
            } => {
                assert_eq!(*player_id, 1, "player_id must be 1");
                assert_eq!(*health_gain, 8, "health_gain must be 8");
                assert_eq!(*interval_ms, 2000, "interval_ms must be 2000");
                assert_eq!(*duration_ms, 60000, "duration_ms must be 60000");
                assert_eq!(*total_heal_cap, 240, "total_heal_cap must be 240");
            }
            other => panic!("expected GameAction::Feed, got {:?}", other),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn do_feed_with_zero_values_still_queues_action() {
        // Edge case: zero health_gain or duration should still queue a Feed action.
        let dir = test_dir("do_feed_zero");
        fs::write(
            dir.join("test.lua"),
            br#"
            function onUse(args)
                do_feed(args.player_id, 0, 0, 0, 0)
                return true
            end
            "#,
        )
        .unwrap();
        let rt = LuaRuntime::new(&dir);
        rt.dispatch("onUse", &sample_args()).unwrap();
        let actions = rt.drain_actions();
        assert_eq!(
            actions.len(),
            1,
            "zero-value feed must still queue an action"
        );
        match &actions[0] {
            GameAction::Feed {
                player_id,
                health_gain,
                ..
            } => {
                assert_eq!(*player_id, 1);
                assert_eq!(*health_gain, 0);
            }
            other => panic!("expected GameAction::Feed, got {:?}", other),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------------------
    // EAT: do_send_text_message binding pushes GameAction::TextMessage
    // ------------------------------------------------------------------

    #[test]
    fn do_send_text_message_queues_textmessage_action() {
        // EAT-04: RED — do_send_text_message(pid, 13, "Glup") must push a
        // GameAction::TextMessage with matching fields.
        let dir = test_dir("do_text_msg");
        fs::write(
            dir.join("test.lua"),
            br#"
            function onUse(args)
                do_send_text_message(args.player_id, 13, "Glup")
                return true
            end
            "#,
        )
        .unwrap();
        let rt = LuaRuntime::new(&dir);
        rt.dispatch("onUse", &sample_args()).unwrap();
        let actions = rt.drain_actions();
        assert_eq!(actions.len(), 1, "exactly one action must be queued");
        match &actions[0] {
            GameAction::TextMessage {
                player_id,
                message_type,
                text,
            } => {
                assert_eq!(*player_id, 1, "player_id must be 1");
                assert_eq!(*message_type, 13, "message_type must be 13");
                assert_eq!(text, "Glup", "text must be 'Glup'");
            }
            other => panic!("expected GameAction::TextMessage, got {:?}", other),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn do_send_text_message_without_call_does_not_queue_action() {
        // Triangulation: a script that does NOT call do_send_text_message
        // must NOT queue a TextMessage action.
        let dir = test_dir("text_msg_skipped");
        fs::write(
            dir.join("test.lua"),
            b"function onUse(args) return true end",
        )
        .unwrap();
        let rt = LuaRuntime::new(&dir);
        rt.dispatch("onUse", &sample_args()).unwrap();
        let actions = rt.drain_actions();
        assert!(
            actions.is_empty(),
            "no actions must be queued when do_send_text_message is not called"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn do_send_text_message_multiple_calls_queue_multiple_actions() {
        // Triangulation: calling do_send_text_message twice must queue
        // two TextMessage actions.
        let dir = test_dir("text_msg_multi");
        fs::write(
            dir.join("test.lua"),
            br#"
            function onUse(args)
                do_send_text_message(args.player_id, 13, "Glup")
                do_send_text_message(args.player_id, 13, "Chomp")
                return true
            end
            "#,
        )
        .unwrap();
        let rt = LuaRuntime::new(&dir);
        rt.dispatch("onUse", &sample_args()).unwrap();
        let actions = rt.drain_actions();
        assert_eq!(actions.len(), 2, "exactly 2 actions must be queued");
        match &actions[0] {
            GameAction::TextMessage { text, .. } => {
                assert_eq!(text, "Glup", "first message must be 'Glup'");
            }
            other => panic!("expected TextMessage, got {:?}", other),
        }
        match &actions[1] {
            GameAction::TextMessage { text, .. } => {
                assert_eq!(text, "Chomp", "second message must be 'Chomp'");
            }
            other => panic!("expected TextMessage, got {:?}", other),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn do_feed_is_not_called_when_script_does_not_invoke_it() {
        // Script must NOT queue a Feed action when do_feed is not called.
        let dir = test_dir("do_feed_skipped");
        fs::write(
            dir.join("test.lua"),
            b"function onUse(args) return true end",
        )
        .unwrap();
        let rt = LuaRuntime::new(&dir);
        rt.dispatch("onUse", &sample_args()).unwrap();
        let actions = rt.drain_actions();
        assert!(
            actions.is_empty(),
            "no actions must be queued when do_feed is not called"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------------------
    // ISSUE 1: Namespaced dispatch (food.onUse, teleport.onUse)
    // ------------------------------------------------------------------

    #[test]
    fn dispatch_namespaced_looks_up_table_method() {
        // dispatch_namespaced("food.onUse", args) must look up the `food`
        // global table and call `food.onUse(args)`.
        let dir = test_dir("ns_table_method");
        fs::write(
            dir.join("food.lua"),
            br#"food = {}
            function food.onUse(args)
                return args.item_id == 1386
            end"#,
        )
        .unwrap();
        let rt = LuaRuntime::new(&dir);
        let args = LuaArgs {
            player_id: 1,
            item_id: 1386,
            pos_x: 100,
            pos_y: 100,
            pos_z: 7,
            stackpos: 0,
        };
        let result = rt.dispatch_namespaced("food.onUse", &args).unwrap();
        assert!(result, "namespaced dispatch must find and call food.onUse");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dispatch_namespaced_returns_false_for_missing_table() {
        // When the table doesn't exist, dispatch_namespaced returns Ok(false).
        let dir = test_dir("ns_missing_table");
        fs::write(dir.join("food.lua"), b"food = {}").unwrap();
        let rt = LuaRuntime::new(&dir);
        let result = rt
            .dispatch_namespaced("nonexistent.onUse", &sample_args())
            .unwrap();
        assert!(!result, "missing table must yield Ok(false)");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dispatch_namespaced_returns_false_for_missing_method() {
        // When the table exists but the method doesn't, returns Ok(false).
        let dir = test_dir("ns_missing_method");
        fs::write(dir.join("test.lua"), b"food = {}").unwrap();
        let rt = LuaRuntime::new(&dir);
        let result = rt
            .dispatch_namespaced("food.onUse", &sample_args())
            .unwrap();
        assert!(!result, "table without method must yield Ok(false)");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dispatch_namespaced_falls_back_to_flat_for_no_dot() {
        // A script name without a dot falls back to flat dispatch.
        let dir = test_dir("ns_fallback");
        fs::write(
            dir.join("test.lua"),
            b"function onUse(args) return true end",
        )
        .unwrap();
        let rt = LuaRuntime::new(&dir);
        let result = rt.dispatch_namespaced("onUse", &sample_args()).unwrap();
        assert!(result, "flat name must fall back to regular dispatch");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dispatch_namespaced_two_scripts_coexist() {
        // Both food and teleport tables can coexist in the same Lua state.
        let dir = test_dir("ns_coexist");
        fs::write(
            dir.join("food.lua"),
            br#"food = {}
            function food.onUse(args)
                do_feed(args.player_id, 8, 2000, 60000, 240)
                return true
            end"#,
        )
        .unwrap();
        fs::write(
            dir.join("teleport.lua"),
            br#"teleport = {}
            function teleport.onUse(args)
                do_teleport(args.player_id, 100, 100, 7)
                return true
            end"#,
        )
        .unwrap();
        let rt = LuaRuntime::new(&dir);

        // Dispatch food.onUse → should queue a Feed action
        let food_args = LuaArgs {
            player_id: 1,
            item_id: 2666,
            pos_x: 100,
            pos_y: 100,
            pos_z: 7,
            stackpos: 0,
        };
        let result = rt.dispatch_namespaced("food.onUse", &food_args).unwrap();
        assert!(result, "food.onUse must dispatch successfully");

        // Dispatch teleport.onUse → should queue a Teleport action
        let tele_args = LuaArgs {
            player_id: 1,
            item_id: 1386,
            pos_x: 100,
            pos_y: 100,
            pos_z: 7,
            stackpos: 0,
        };
        let result = rt
            .dispatch_namespaced("teleport.onUse", &tele_args)
            .unwrap();
        assert!(result, "teleport.onUse must dispatch successfully");

        // Drain and verify both actions are present
        let actions = rt.drain_actions();
        assert_eq!(actions.len(), 2, "both scripts should have queued actions");

        let has_feed = actions.iter().any(|a| matches!(a, GameAction::Feed { .. }));
        let has_tele = actions
            .iter()
            .any(|a| matches!(a, GameAction::Teleport { .. }));
        assert!(has_feed, "food.onUse must have queued a Feed action");
        assert!(
            has_tele,
            "teleport.onUse must have queued a Teleport action"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
