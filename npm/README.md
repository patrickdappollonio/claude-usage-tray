# @patrickdappollonio/claude-usage-tray

A minimal tray icon that shows your Claude Code subscription usage: the 5-hour
session percentage and the 7-day weekly percentage, with reset times. It lives
in the Linux system tray (StatusNotifierItem) and in the macOS menu bar.

This package ships prebuilt binaries for `linux-x64`, `linux-arm64`,
`darwin-x64` and `darwin-arm64`. The Linux ones are statically linked.

## Usage

Run it without installing:

```sh
npx @patrickdappollonio/claude-usage-tray
```

Or install it globally:

```sh
npm install -g @patrickdappollonio/claude-usage-tray
claude-usage-tray hook install   # wire up the Claude Code statusline
claude-usage-tray                # run the tray
```

`hook install` records the **absolute path** of the binary it was run from, so
re-run it after upgrading the package.

## Requirements

- Linux with a StatusNotifierItem host: KDE Plasma, XFCE 4.16+, LXQt, Cinnamon
  or MATE work out of the box. GNOME needs the
  [AppIndicator and KStatusNotifierItem Support](https://extensions.gnome.org/extension/615/appindicator-support/)
  extension.
- Or macOS 12 or newer, where the icon goes in the menu bar. Notifications work
  but are basic, because the binary is not shipped inside an app bundle.
- Claude Code >= 2.1.80 (the version that added `rate_limits` to the statusline
  JSON), on a Pro/Max subscription account.

## No network calls, no credentials

This tray makes zero network requests and never reads your Claude credentials.
It only reads a local JSON file — the data Claude Code itself already hands to
your statusline on stdin.

Full documentation:
<https://github.com/patrickdappollonio/claude-usage-tray>

## License

MIT. See `LICENSE`.
