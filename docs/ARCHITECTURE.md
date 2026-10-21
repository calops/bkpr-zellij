This document details the different components of the code base in plain English. The intention is to provide a high level description rather than a more detailed drill-down of each internal mechanism.

## Screen (zellij-server/src/screen.rs)
The Screen is responsible for managing the relationship between the panes currently displayed on screen.
It is in charge of actions such as: 
  * Coordinating the resizing of panes
  * Creating new panes
  * Closing existing panes (and filling up their space by other panes)

## Terminal Pane (zellij-server/src/panes/terminal_pane.rs)
The TerminalPane represents a pane on screen that connects to a single pty (pseudo terminal) and (presumably) has one shell or other program running inside it.
The TerminalPane has two main roles:
  * Keeping track of the Scroll, which represents the buffer of lines in this terminal
  * Interpreting the ANSI/VT instructions coming from the pty in order to adjust the styling of characters, change the cursor position, etc.

### Scroll (zellij-server/src/panes/terminal_pane.rs and zellij-server/src/panes/grid.rs)
The Scroll holds the terminal buffer and is in charge of:
  * Keeping track of the viewport (which part of it we see) this can change when we scroll up/down
  * Keeping track of the cursor position
  * Controlling line-wrapping

### Terminal Character (zellij-server/src/panes/terminal_character.rs)
The `TerminalCharacter` represents a single character in the pane. It holds the char itself on the one hand, and an internal `CharacterStyles` struct representing the styling of this character.
This struct derives the `Copy` trait for performance reasons, because it is moved around quite a bit (eg. when line wrapping).

### How a terminal emulator draws characters?
The ANSI/VT instructions received from the pty include style instructions (eg. change foreground colour to red) as well as instructions to reposition the cursor (eg. go to line x/y) and to perform certain actions with the current buffer on-screen (eg. delete all lines after cursor).

When an instruction is received to change the character style, we know that all characters printed from now on should use that style. This is true until we move to a new line (with a newline instruction or a go to instruction) or until we receive a style reset code for this specific style. There are several kinds of reset codes that apply to different styles.

It's important to understand that these styles are ongoing relative to the current cursor position. It might be useful to imagine them as brushes: "pick up the red brush", "put down the bold brush", etc.

This is important to note because the styles themselves are saved only on characters that have already been printed. If we receive an instruction to change the style to blue, then print a (blue) character, then receive another instruction to move to a new line, print another (plain) character and then come back, the style would be reset. We would have to receive a new instruction to change the style to blue in order for the next character to be blue.

## Boundaries (zellij-server/src/ui/boundaries.rs)
The Boundaries refer to those lines that are drawn between terminal panes. A few things that happen here:
  * The Rect trait is here so that different panes can implement it, giving boundaries a generic way to calculate the size of the pane and draw boundaries around it.
  * Here we use the [unicode box drawing characters](https://en.wikipedia.org/wiki/Box-drawing_character) in order to draw the borders. There's some logic here about combining them together for all possible combinations of pane locations.

## PTY Bus (zellij-server/src/pty.rs)
The PtyBus keeps track of several asynchronous streams that read from pty sockets (eg. /dev/pts/999), parse those bytes into ANSI/VT events and send them on to the Screen so that they can be received in the relevant TerminalPane.

## Bkpr pane context (zellij-server/src/pane_context.rs)

Bkpr launches opt in with host-only marker `BKPR_NATIVE_CONTEXT=1`, plus
`BKPR_SESSION_ID` and `BKPR_SESSION_NAME`.
The launcher must reuse the opaque session ID only when attaching to the same
live server, and allocate a fresh ID when an ended session name is reused.
Screen owns the live tab/pane topology; terminal reservation captures the
destination's immutable context before command construction, outside fork/exec.
This covers direct commands, layout panes, stock splits, held-command reruns
and drop-to-shell. Closed destinations cancel cleanly. Layout overrides retain
pending tab bindings across Screen synchronization, but publish them only after
Screen installs the tab.
The Unix exec boundary clears inherited context and installs `BKPR_SESSION_ID`,
`BKPR_SESSION_NAME`, `BKPR_WORKSPACE_ID`, `BKPR_WORKSPACE_NAME` and
`BKPR_PANE_ID=terminal_N`. Existing processes retain their launch snapshot.
Sessions without the bootstrap keep ordinary Zellij behavior, except that child
processes have inherited reserved Bkpr context cleared (important for nesting).

Stock tabs receive a canonical workspace before their first process starts.
A session-scoped creation counter prevents a reused native tab ID from reusing
a closed workspace identity.
Tab KDL properties `bkpr_workspace_id` and `bkpr_workspace_name` (both nonempty)
explicitly join a Bkpr workspace, allowing several native tabs to share one
workspace. Native moves update live membership; stock tab renames update the
default workspace name. Explicit groups retain their separately owned name.

Plugins with ReadApplicationState receive `CustomMessage("bkpr_context", json)`
with `version:1`, `session_id`, `session_name`, `tabs` (each containing `tab_id`,
`workspace_id`, `workspace_name`) and `panes` (each containing `pane_id`
(`terminal_N` or `plugin_N`), `tab_id`, `workspace_id`, `workspace_name`,
optional `root_pid`).
This is emitted with native topology and on subscription/permission bootstrap,
without a new plugin SDK or protobuf event. Consumers must use this live view
after moves and renames rather than immutable inherited environment variables.

The native owner can post `bkpr_context_update` through the existing
post-message-to-plugin API. Its JSON is `{session_id,generation,tabs:[...]}`;
the host requires ChangeApplicationState, configuration role `native-owner`,
and configuration `context_owner` matching the private token retained from the
session's original `load_plugins` configuration. Generations must strictly increase; replacement owners have
higher plugin IDs and supersede earlier owners. Foreign sessions, stale owners,
stale generations and nonexistent tabs are rejected. The token is excluded
from terminal/background child environments and the session-environment API.
Before asynchronous execution, a trusted owner acquires the exclusive
`owner-PLUGIN_ID.lock` lease in the launcher's existing private context session
directory. The regular file is current-user-owned, `0600`, no-follow and
CLOEXEC. Unload, loading failure, crash or replacement drops the FD immediately;
the mere existence of a stale lock file is not liveness. Bkpr snapshot readers
and delayed publishers require that exact owner's lease to remain externally
locked, in addition to their server-birth and sequence checks.
A sidebar can request `bkpr_context_owner_restart` with `{session_id,closed_owner}`.
The host checks its role, session and actual plugin source, reloads the retained
owner configuration only after that owner has unloaded, and deduplicates pending
restart requests. No authority token is returned to sidebars or supplied in env.
The opt-in marker is also removed from all child environments and session-env
readback, so an ordinary nested Zellij does not inherit Bkpr host authority.

