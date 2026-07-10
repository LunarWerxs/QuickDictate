# Settings sync (Connections)

QuickDictate can optionally back up and sync your **portable preferences** —
things like mode, language, hotkeys, and STT provider/model — across the
machines you use, via a LunarWerx Connections account.

**This feature is opt-in and off by default.** If you never click "Sync
settings with Connections," nothing about how QuickDictate runs changes:
settings stay local, and no account or network activity for this feature ever
happens.

## What it does

A **"Settings sync"** card lives in the Settings window. Click *Sync settings
with Connections* and your system browser opens a normal sign-in page
(OAuth Authorization Code + PKCE) — your password never touches the app
itself. Once signed in:

- Your portable preferences are pulled down on Settings-window open (so any
  changes made on another machine show up automatically) and on first sign-in.
- Your preferences are pushed up whenever you hit **Save** (or **Save &
  Restart**).
- Clicking **Stop syncing** disconnects the account, deletes the synced copy
  from the server, and drops the local sign-in — everything reverts to
  local-only.

## What syncs and what never does

- **Syncs:** portable preferences only — mode, language, toggle/hold hotkeys,
  timing tweaks, output formatting toggles, sound/close-behavior settings,
  STT provider and model choice, text replacements, and similar app-behavior
  settings.
- **Never syncs:** your **API keys**, microphone **audio**, or **transcripts**.
  Also excluded: window position/size and other per-machine or local-only
  settings (e.g. `run_at_startup`, logging toggles), since those don't make
  sense to carry between machines.

Your API keys, dictation audio, and recognized text never leave your machine
because of this feature — it only ever reads and writes the small set of
preference fields listed above.

## How to turn it on

1. Open **Settings**.
2. Find the **Settings sync** card and click **Sync settings with
   Connections**.
3. Sign in via the browser window that opens.
4. Your preferences sync automatically from then on, on Settings-window open
   and on Save.

## How to turn it off

Open **Settings**, find the **Settings sync** card, and click **Stop
syncing**. This removes your synced preferences from the server and signs
the app out locally — QuickDictate goes back to fully local-only operation.

See [.github/SECURITY.md](../.github/SECURITY.md) for the full data-handling
policy.
