# Wake's Slipstream plugin

A card beside the clock: `2 waiting · 3 working · 5 ideas`, the session waiting longest and its
project, and a **Go to it** button. Typing `@` and a few words in the explorer offers
"Go to <session>". An action, **Go to the agent session waiting longest**, can be given keys in
Settings → Shortcuts.

It asks for no `needs`, so Settings rates it **Safe**. It reads one folder, `$XDG_DATA_HOME/wake/card`,
and writes only its own state folder.

## What Wake writes: `~/.local/share/wake/card/card.json`

A folder of its own, not `~/.local/share/wake/` itself: Slipstream checks every file in a `read`
folder before each run and stops a plugin whose folder holds more than 4096, which Wake's
`index/sessions/` will pass, and the card has no business seeing transcripts' summaries.

Wake writes it atomically (temp file and rename) every 15 seconds while it runs; the card says
"Wake has not updated this for a while" after 120 s:

```json
{"working": 3, "waiting": 2, "idle": 4, "ideas": 5,
 "top": {"id": "<sessionId>", "title": "Teacher invites", "project": "Gradebook"},
 "sessions": [{"id": "<sessionId>", "title": "…", "project": "…", "state": "waiting"}],
 "updatedAt": "2026-11-19T22:00:00Z"}
```

- `top` is the session waiting longest, or `null`.
- `sessions` is for explorer answers: live sessions, waiting first, then working, then idle; at
  most 50. Ids must match `[A-Za-z0-9_-]{1,80}`.

## What Wake reads: jump requests

A sandboxed plugin has no session bus, so it cannot bring a window forward or send notifications.
A press (the button, an explorer answer or the action's keys) instead writes
`~/.local/state/slipstream/plugins/wake/state/request.json`:

```json
{"id": "<random hex>", "action": "jump", "session": "<sessionId>", "at": "2026-11-19T22:00:00Z"}
```

While its window is open, Wake checks that file once a second (it reads it only when it has
changed) and acts once per `id`: it ignores a request more than 30 s old, or for a session it
doesn't know, and otherwise goes there as `o` does. Wake never deletes the file; it remembers
the last `id` it handled in `~/.local/share/wake/` instead.

The path assumes the plugin is installed under the id `wake`, i.e. as a folder named `wake`
(copy this folder to `~/.local/share/slipstream/plugins/wake/`, then switch it on in
Settings → Plugins). The folder must hold no links.

## Not included: an "Open Wake" button

A card button can only run this program inside its sandbox, or open an `https://` address with
the `open` need, and the sandbox has no display or session bus to start Wake with. Only
`needs = ["everything"]` would allow it, which is far too much for a button. Give Wake a
shortcut in Settings → Shortcuts instead.

## Trying it

```sh
SLIPSTREAM_PLUGIN_READ_0=/path/to/folder/with/card.json SLIPSTREAM_PLUGIN_STATE=/tmp/x ./wake-card
./wake-card search invites
./wake-card press top
```
