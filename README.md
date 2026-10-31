# RustCommander

A fast, keyboard-driven, dual-panel file manager in the spirit of Total
Commander, with a modern minimalist dark look. Built with Rust and
[iced](https://iced.rs).

## Build & run

```sh
cargo run -p rustcommander          # debug
cargo run -p rustcommander --release
cargo test                          # core tests
```

## Layout

| Crate | Purpose |
|---|---|
| `crates/core` (`rc-core`) | UI-agnostic logic: `Panel` (listing, cursor, selection, sorting), `Entry`, `Command`, `ops` (copy/move/delete jobs). No UI dependencies. |
| `crates/platform` (`rc-platform`) | OS integration via `windows-rs`: drives, disk space, system icons, Explorer context menu, launching files. |
| `crates/ui` (`rustcommander`) | iced front end: `app/mod.rs` (state/update), `app/view.rs` (drawing), `keymap.rs` (keys → `Command`), `theme.rs` (colours/styles). |

Every action is a `rc_core::Command` (`cm_*` ids like Total Commander), so key
bindings, the F-key bar and a future command palette share one code path.

## Keys

| Key | Action |
|---|---|
| ↑ ↓ PgUp PgDn Home End | Move cursor |
| Enter / Ctrl+PgDn | Open folder / file |
| Backspace / Ctrl+PgUp | Parent folder |
| Ctrl+\\ | Root |
| Tab | Switch panel |
| Insert / Space | Select (Insert moves down) |
| Ctrl+A / Ctrl+D / Esc | Select all / deselect |
| Ctrl+F3–F6 | Sort by name / ext / date / size (again = reverse) |
| Ctrl+R / Ctrl+F2 | Refresh |
| Ctrl+H | Toggle hidden/system files |
| Alt+O | Show this folder in the other panel |
| F3 / F4 | View / edit (opens with system default for now) |
| F5 / F6 | Copy / move selection (or cursor item) to the other panel; target is editable, relative paths allowed, a single item can be given a new name |
| F2 / Shift+F6 | Rename in place |
| F7 | New folder (nested paths like `a\b` work) |
| F8 / Del | Delete to Recycle Bin |
| Shift+F8 / Shift+Del | Delete permanently |
| Esc | Close dialog / cancel job at an overwrite prompt |

## Mouse

- **Drive bar**: click a drive to jump to it; free space is shown on the right.
- **Path bar**: click to edit the path (absolute, relative, `~`, or a file path to jump to that file). Enter to go, Esc to cancel.
- **Drag and drop**: drag files onto the other panel to copy (hold Shift to move). Drop onto a folder row to copy into that folder. A confirmation dialog with the target follows.
- **Right-click**: the Windows Explorer context menu for the item(s) (Open with, Send to, 7-Zip, …); on empty space, the folder's menu (New, Open in Terminal, …). Rename and Delete in the menu use RustCommander's own dialogs.
- File and folder icons come from the system.

File operations run as background jobs (several at once) with progress,
speed, pause/resume and cancel. Existing files trigger an overwrite prompt:
overwrite, overwrite all, overwrite older, skip, skip all, or cancel. Moves
use a plain rename when possible and fall back to copy and delete across
drives. Skipped files stay in the source.

## Roadmap

1. ~~File operations with background jobs, progress and conflict prompts~~ ✔
2. Async directory loading + filesystem watching (`notify`)
3. Tabs per panel, history, hotlist (Ctrl+D)
4. Quick search (type to jump), quick filter
5. Built-in viewer (F3), multi-rename, compare by content
6. `Vfs` trait: archives, SFTP/FTP, plugins
7. Windows shell integration: ~~context menu, icons, recycle bin~~ ✔, drag out to / in from Explorer
8. Config file for keymap and theme
