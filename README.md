# rmnm

A terminal launcher for [Monsters & Memories](https://monstersandmemories.com), built with
[ratatui](https://ratatui.rs). It signs in to your account, shows the News feed and the patch
notes, patches and repairs the game, and starts it through
[umu](https://github.com/Open-Wine-Components/umu-launcher).

## Build and run

```sh
cargo run --release
```

The mouse works on tabs, list rows and buttons. Keys are listed in the status line.

## Patch and Repair

- **Patch** downloads the current build's manifest, then checks each file it lists. A file is
  trusted if its size, modification time and hash still match what rmnm last verified;
  otherwise it is hashed again. Anything missing or wrong is downloaded and rebuilt.
- **Repair** does the same but re-hashes every file.
- **Reinstall** deletes the game files (every file the manifest lists, plus any file rmnm
  recorded from an earlier build), the downloaded bundles and rmnm's records of verified files,
  then downloads the whole game again. Use it after a build is rolled back.

Each of the three asks the server for the current build before it starts, so a build that was
replaced or rolled back while rmnm was open is never installed.

Bundles are downloaded over HTTPS, four at a time. Every bundle and every chunk is checked
against its CRC-64, and every file against its XXH3-128. A rebuilt file replaces the old one
only after it passes. Cancelling keeps the bundles that finished downloading, and the next run
reuses them; Reinstall deletes them. Files in the install folder that the manifest does not
list, such as `game.db` and `Screenshots/`, are reported and left alone.

## Play

Play checks the session with the server, then runs:

```
<umu-run command> <install>/mnm.exe <game arguments> --token <token>
```

The working directory is the install folder. The environment adds `GAMEID`, `WINEPREFIX` (from
`MNM_WINEPREFIX`), `PROTONPATH` (from `MNM_PROTONPATH`, when set) and every enabled extra
variable; extra variables can override the first three. Variables an AppImage leaks into its
children (`PYTHONHOME`, `LD_LIBRARY_PATH`, …) are removed, because they break umu's Python.
The game's output goes to `$XDG_DATA_HOME/rmnm/logs/game.log`, and the previous run's log is
kept as `game.log.1`. The token never appears in the log. The game keeps running if rmnm quits.

The game accepts the token only on its command line, so other local users can see it in
`ps` while the game runs.

## Where things live

| What | Default |
|---|---|
| rmnm's database (settings, environment variables, session token) | `$XDG_DATA_HOME/rmnm/rmnm.db`, mode 0600 |
| Game files | `$XDG_DATA_HOME/rmnm/mnm` |
| Wine prefix (`MNM_WINEPREFIX`) | `~/Games/umu/<GAMEID>`, umu's own default |

Change any of these on the Settings tab. `$XDG_DATA_HOME` is normally `~/.local/share`.

On first start rmnm looks for the official launcher's `launcher.db` (in
`$XDG_DATA_HOME/com.monstersandmemories.launcher/`). If it finds a token the server still
accepts, it signs in with that token. The file is opened read-only. Settings → "Import token"
repeats the import whenever you want.

## License

MIT; see [LICENSE](LICENSE).
