# Task: Refactor Rust Music Player Codebase for Safety, Architecture, and Performance

## Context
You are working on a Rust music player project using Slint UI. The codebase consists of ~4750 lines across 11 files. 
Key components:
- `src/app.rs`: Main application logic (Monolithic, ~1400 lines)
- `src/audio/player.rs`: Audio playback engine with callbacks
- `src/playlist.rs`: Playlist management
- `src/settings.rs`: Configuration handling
- `src/tray.rs`: System tray integration

## Objective
Analyze the current codebase and apply specific refactoring to address critical safety issues, architectural debt, and performance bottlenecks identified in a recent code review.

## Critical Issues to Fix (Priority 1)

### 1. Eliminate Unsafe `.unwrap()` Calls
There are 30+ instances of `.unwrap()` that can cause panics. Replace them with proper error handling (`Result`, `Option`, or graceful logging).
**Specific targets:**
- `app.rs`: `hex_color()` function (lines ~53-65) - currently panics on invalid hex strings.
- `app.rs`: `self.current.unwrap()` (line ~1090) - panics if no track is selected.
- `audio/player.rs`: `self.decoder.as_mut().unwrap()` inside audio callbacks (line ~65).
- `audio/player.rs`: Multiple `lock().unwrap()` calls in audio callbacks (lines 173, 196, 215, etc.) which risk deadlocks.

### 2. Secure `hex_color()` Implementation
The current implementation is unsafe.
**Requirement:** Rewrite `hex_color(&str) -> Option<slint::Color>` to:
- Validate string length (must be 6 or 8 chars after trimming '#').
- Validate characters (must be valid hex digits).
- Handle UTF-8 conversion errors gracefully.
- Return `None` instead of panicking on invalid input.

### 3. Prevent Audio Thread Deadlocks
In `src/audio/player.rs`, using `Mutex::lock().unwrap()` inside real-time audio callbacks is dangerous.
**Requirement:** 
- Replace `lock().unwrap()` with `try_lock()` in time-critical audio paths.
- If locking fails, skip the update for that frame rather than blocking the audio thread.
- Ensure no heavy operations occur while holding locks in the audio callback.

## Architectural Improvements (Priority 2)

### 4. Decompose `src/app.rs`
The `MusicApp` struct is a "God Object" (~1400 lines). Split it into logical modules:
- Create `src/app/ui_manager.rs`: Handle UI bindings, `sync_playlist_to_ui`, and column widths.
- Create `src/app/playback_manager.rs`: Handle player control, track changes, and state updates.
- Create `src/app/playlist_manager.rs`: Handle track list logic and model updates.
- Update `src/app/mod.rs` to re-export these and coordinate them.

### 5. Unify Channel Types
Currently, the code mixes `std::sync::mpsc` and `tokio::sync::mpsc`.
**Requirement:** Standardize on one channel type (preferably `tokio::sync::mpsc` since the project uses tokio) for all internal communication (Tray, UI updates, Audio events) to avoid context switching issues and complexity.

### 6. Optimize Playlist Sync
`sync_playlist_to_ui()` currently recreates the entire UI model on every change.
**Requirement:** Implement incremental updates. Only modify the rows that have changed, added, or removed, rather than rebuilding the whole `Vec<StandardListViewItem>`.

## Code Quality & Maintenance (Priority 3)

### 7. Add Documentation
Add standard Rust doc comments (`///`) to all public structs, enums, and functions, especially in `player.rs` and `app.rs`.

### 8. Remove Magic Numbers
Extract hardcoded values into constants:
- `TRAY_UPDATE_INTERVAL_MS = 300`
- `SCAN_BATCH_SIZE = 200`
- Any other hardcoded timeouts or limits.

### 9. Reduce Callback Cloning
Review `bind_callbacks()` in `app.rs`. Minimize unnecessary `Rc::clone()` or `Arc::clone()` inside closure definitions where a reference or weaker pointer might suffice.

## Expected Output
1. **Refactored Code**: Apply these changes directly to the files.
2. **Safety First**: Ensure no new panics can be triggered by user input or race conditions.
3. **Modularity**: `app.rs` should be significantly smaller and delegate logic to new modules.
4. **Verification**: Ensure the project still compiles and passes existing tests after changes.

Please start by addressing the **Critical Issues (Priority 1)**, specifically the `hex_color` safety and the audio thread locking mechanisms, as these pose the highest risk.