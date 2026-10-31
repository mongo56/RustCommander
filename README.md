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
| `crates/core` (`rc-core`) | UI-agnostic logic: `Panel` (listing, cursor, selection, sorting), `Entry`, `Command`. No UI dependencies. |
| `crates/platform` (`rc-platform`) | OS integration: drives, opening files. Later: shell context menu, icons, recycle bin, drag-out. |
| `crates/ui` (`rustcommander`) | iced front end: `app.rs` (state/update/view), `keymap.rs` (keys → `Command`), `theme.rs` (colours/styles). |

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
| F5–F8 | Copy / Move / MkDir / Delete (not implemented yet) |

## Roadmap

1. File operations (copy/move/delete/mkdir) on a background job queue with progress and conflict prompts
2. Async directory loading + filesystem watching (`notify`)
3. Tabs per panel, history, hotlist (Ctrl+D)
4. Quick search (type to jump), quick filter
5. Built-in viewer (F3), multi-rename, compare by content
6. `Vfs` trait: archives, SFTP/FTP, plugins
7. Windows shell integration: context menu, icons, drag & drop, recycle bin
8. Config file for keymap and theme
