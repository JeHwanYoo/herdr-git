# Architecture

## Module rules

- Use `name.rs` as a module entry point and `name/` for its child modules.
- Name modules by responsibility and use snake_case.
- Keep child modules private; re-export items needed outside their parent module from the parent entry point.
- Within a parent module, its children may use each other directly.
- For new items, use the narrowest visibility that reaches their callers.

## Source layout

```text
src/
├── git/
├── git.rs      Git processes, parsing, repository data, and operations
├── herdr/
├── herdr.rs    Herdr integration
├── project/
├── project.rs  Project registration, persistence, and worktree inspection
├── ui/
└── ui.rs       App construction and the event loop
```

## Module dependencies

```text
herdr ←── ui ──→ project
          │         │
          ↓         │
         git ←──────┘
```

Only the dependencies shown above are allowed.

## UI

Module names below refer to children of `ui`; App is defined in `ui.rs`.

| Module | Responsibility |
| --- | --- |
| `ui.rs` | App construction and the event loop |
| `view.rs` | Screen layout |
| `shell.rs` | Global navigation and shortcuts |
| `overlay.rs` | Active dialog state, input dispatch, and rendering dispatch |
| `review.rs` | Code selection, copy dialogs, and copied-text formatting |
| `commands.rs` | Command workflows and availability |
| `effect.rs` | Requests, results, and workers for foreground jobs and repository refreshes |
| `lanes.rs` | Job scheduling, read cancellation, and refresh result application to App |
| `history.rs` | History paging and maintenance workers, including scheduling and shutdown |
| `widgets.rs` | Reusable UI controls and geometry |
| `theme.rs` | Visual styles |

- Keep feature-specific state, input handling, rendering, and dialog behavior together, as in `files` and `workspaces`.
- Feature modules may implement App methods; App coordinates their work.
- Run foreground read jobs and mutation jobs on separate workers. Workers in `effect` and `history` exchange requests and results with the UI thread and must not access App.
- Dispatch repository reads, Git commands, and project persistence to background workers from UI handlers; render from in-memory state. Terminal input and drawing stay on the UI thread.
- Before applying an asynchronous result, verify that it belongs to the current request and repository. Discard superseded results.
- `widgets` may depend on `theme`. `theme` must not depend on features or widgets.

## Extending features and testing

- Add behavior and its types to the module responsible for that operation or data. Create a sibling module when the new behavior falls outside the responsibilities of existing modules at that level.
- A feature may stay in one file. Split out a child module when a group of functions and types can hide implementation details behind an interface used by the parent.
- Extract shared code for existing callers that need the same behavior; similar-looking code alone does not require an abstraction.
- Keep unit tests in the module they test; put fixtures shared by UI tests in `src/ui/test_support.rs`.
- Run `cargo fmt --check` and tests covering the changed behavior. Run the full test suite when changing shared Git behavior, worker coordination, or cross-feature state.
- Add persistent tests when the risk of recurrence and its impact justify their maintenance cost. Verify other fixes with focused checks during the change.
- Work within the existing boundaries where possible. If a feature requires changing responsibilities or dependencies, update this document in the same change and explain why in your final response. Create additional documentation files only when requested.
