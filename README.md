# litelaunch-vd

Video downloader (yt-dlp), a built-in plugin of [LiteLaunch](https://github.com/Slava-Gusak/litelaunch)
(`vd <link>`). It replaces Flow's AnyVideo Downloader and lives in the launcher as the submodule
`plugins/vd`.

- `vd.rs`: the native part, compiled into the launcher. It checks the link and caches results per link for
  an hour. If yt-dlp is missing it offers to install it with pip. It can prewarm Python
  (`prewarm`, off by default). It embeds the files below and unpacks them to `%APPDATA%\LiteLaunch\vd`.
- `vd.py`: the worker. It speaks the Flow JSON-RPC v1 protocol, so it runs through the launcher's
  plugin machinery.
  - The query saves yt-dlp's unprocessed info, and the download reuses it without extracting again.
    Processed info gets 403 on YouTube https formats.
  - YouTube is asked with the `visionos` client alone: ~2 s instead of ~4.5 s. If that returns nothing,
    the default clients are used.
  - yt-dlp updates itself weekly in the background.
- `icon.png` (results) and `vd.ico` (notifications, resource 3 in the launcher's `app.rc`).
