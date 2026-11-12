# litelaunch-vd

Video downloader (yt-dlp), a built-in plugin of [LiteLaunch](https://github.com/LiteLaunch/litelaunch)
(`vd <link>`). It lives in the launcher as the submodule `plugins/vd`.

It takes over from [AnyVideo Downloader](https://github.com/z1nc0r3/AnyVideo-Downloader-Flow-Plugin)
by z1nc0r3, the Flow Launcher plugin it replaces, and keeps its `vd` keyword.
The code is written from scratch for LiteLaunch: a native Rust front and a small yt-dlp worker.

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
- `icon.png` (results) and `vd.ico` (notifications, resource 3 in the launcher's `app.rc`), both drawn by
  `make_icon.py`.
