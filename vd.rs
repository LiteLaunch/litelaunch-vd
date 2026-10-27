//! vd — video downloader (yt-dlp). The link is checked
//! and results are cached here; Python (plugins/vd/vd.py) runs only to extract a link and to download.
//! The script is embedded and unpacked to %APPDATA%\LiteLaunch\vd; yt-dlp lives in its lib.

use crate::config::PluginConfig;
use crate::pyplugin::PyResult;
use crate::search::{Action, Item};
use crate::util::{CREATE_NEW_CONSOLE, CREATE_NO_WINDOW, clipboard_text, expand_env};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FILES: &[(&str, &[u8])] = &[("vd.py", include_bytes!("vd.py")), ("icon.png", include_bytes!("icon.png"))];

/// Extracted results stay valid this long (the saved info's links expire after a few hours).
const CACHE_TTL: Duration = Duration::from_secs(3600);
/// A prewarmed Python waits 30 s for the link (vd.py); take it only while it surely still does.
const WARM_TTL: Duration = Duration::from_secs(25);

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct VdConfig {
    pub keyword: String,
    /// Environment variables are allowed.
    pub download_dir: String,
    /// Container for video downloads: mp4, mkv, webm...
    pub video_format: String,
    /// Audio-only downloads: mp3, m4a, opus, flac...
    pub audio_format: String,
    /// "Resolution", "File Size", "Total Bitrate" or "FPS".
    pub sorting_order: String,
    pub open_folder_after: bool,
    pub overwrite: bool,
    /// cookies.txt for sites that need a login; empty for none.
    pub cookie_file: String,
    /// Folder with ffmpeg.exe; empty: next to vd.py or on PATH.
    pub ffmpeg_dir: String,
    /// Start Python as soon as the keyword is typed, so a pasted link skips the ~0.7 s
    /// yt-dlp import. Costs ~50 MB for up to 30 s even if no link comes.
    pub prewarm: bool,
}

impl Default for VdConfig {
    fn default() -> Self {
        Self {
            keyword: "vd".into(),
            download_dir: "%USERPROFILE%\\Downloads".into(),
            video_format: "mp4".into(),
            audio_format: "mp3".into(),
            sorting_order: "Resolution".into(),
            open_folder_after: false,
            overwrite: true,
            cookie_file: String::new(),
            ffmpeg_dir: String::new(),
            prewarm: false,
        }
    }
}

#[derive(Clone)]
pub enum Op {
    OpenFolder,
    InstallLib,
}

pub fn dir() -> PathBuf {
    crate::config::dir().join("vd")
}

pub fn icon() -> String {
    dir().join("icon.png").to_string_lossy().into()
}

/// vd.py speaks the Flow plugin protocol, so it runs through the same plugin machinery.
pub fn plugin_config(cfg: &VdConfig) -> PluginConfig {
    PluginConfig {
        name: "Video Downloader".into(),
        keyword: cfg.keyword.clone(),
        dir: dir().to_string_lossy().into(),
        entry: "vd.py".into(),
        icon: "icon.png".into(),
        settings_file: String::new(),
    }
}

/// Writes the embedded script to %APPDATA%\LiteLaunch\vd (when changed).
pub fn install() {
    crate::util::unpack(&dir(), FILES);
}

pub fn has_lib() -> bool {
    dir().join("lib").join("yt_dlp").exists()
}

/// Extracted results per link.
#[derive(Default)]
pub struct Cache(HashMap<String, (Instant, Vec<PyResult>)>);

impl Cache {
    pub fn get(&self, url: &str) -> Option<&[PyResult]> {
        self.0.get(url).filter(|(at, _)| at.elapsed() < CACHE_TTL).map(|(_, r)| r.as_slice())
    }

    /// Only real results: errors ("couldn't extract") are worth retrying.
    pub fn put(&mut self, url: &str, results: &[PyResult]) {
        self.0.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
        if results.iter().any(|r| r.action.is_some()) {
            self.0.insert(url.to_string(), (Instant::now(), results.to_vec()));
        }
    }
}

/// `https://host/something` with no spaces: what yt-dlp can be asked about.
pub fn clean_url(s: &str) -> Option<String> {
    let s = s.trim();
    let (scheme, rest) = s.split_once("://")?;
    if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")) || s.contains(char::is_whitespace) {
        return None;
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = &rest[..end];
    let target = rest[end..].trim_start_matches('/');
    (host.contains('.') && !target.is_empty()).then(|| s.to_string())
}

pub enum Found {
    Items(Vec<Item>),
    /// Ask yt-dlp about this link.
    Fetch(String),
}

fn item(title: impl Into<String>, sub: impl Into<String>, action: Action) -> Item {
    Item::new(title, sub, Some(icon()), 0, action)
}

pub fn search(cfg: &VdConfig, q: &str, cache: &Cache) -> Found {
    if !has_lib() {
        return Found::Items(vec![item(
            "Установить yt-dlp",
            "Enter запустит pip в отдельном окне",
            Action::Vd(Op::InstallLib),
        )]);
    }
    let folder = expand_env(&cfg.download_dir);
    let open_folder = item("Открыть папку загрузок", folder, Action::Vd(Op::OpenFolder));
    if q.is_empty() {
        let first = match clean_url(&clipboard_text()) {
            Some(url) => item("Ссылка из буфера обмена", url.clone(), Action::SetQuery(format!("{} {url}", cfg.keyword))),
            None => item("Вставьте ссылку на видео", "YouTube, VK, Twitch, TikTok и ещё ~1800 сайтов", Action::Nothing),
        };
        return Found::Items(vec![first, open_folder]);
    }
    let Some(url) = clean_url(q) else {
        return Found::Items(vec![item("Некорректная ссылка", "Ожидается ссылка вида https://сайт/видео", Action::Nothing)]);
    };
    match cache.get(&url) {
        Some(results) => Found::Items(crate::search::py_items(crate::search::PluginRef::Vd, results.to_vec())),
        None => Found::Fetch(url),
    }
}

pub fn run(op: &Op, cfg: &VdConfig, python: &str) {
    match op {
        Op::OpenFolder => {
            let folder = expand_env(&cfg.download_dir);
            let _ = std::fs::create_dir_all(&folder);
            let _ = Command::new("explorer.exe").arg(folder).spawn();
        }
        Op::InstallLib => {
            let lib = dir().join("lib");
            let _ = Command::new(crate::util::console_python(python))
                .args(["-m", "pip", "install", "--target"])
                .arg(lib)
                .arg("yt-dlp[default]")
                .creation_flags(CREATE_NEW_CONSOLE)
                .spawn();
        }
    }
}

/// A Python that has imported yt-dlp and waits for the request on stdin.
pub type Warm = Arc<Mutex<Option<(Child, Instant)>>>;

/// Starts a prewarmed Python unless one is already waiting.
pub fn warm_up(slot: &Warm, python: &str) {
    let mut slot = slot.lock().unwrap();
    if slot.as_mut().is_some_and(|(c, at)| at.elapsed() < WARM_TTL && matches!(c.try_wait(), Ok(None))) {
        return;
    }
    let dir = dir();
    *slot = Command::new(python)
        .arg("-B")
        .arg(dir.join("vd.py"))
        .arg("--wait")
        .current_dir(&dir)
        .env("PYTHONIOENCODING", "utf-8")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()
        .map(|c| (c, Instant::now()));
}

/// The waiting Python, if it is still fresh enough to take a request.
pub fn take_warm(slot: &Warm) -> Option<Child> {
    let (mut c, at) = slot.lock().unwrap().take()?;
    if at.elapsed() < WARM_TTL && matches!(c.try_wait(), Ok(None)) { Some(c) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert!(clean_url("https://www.youtube.com/watch?v=jNQXAC9IVRw").is_some());
        assert!(clean_url("https://youtu.be/jNQXAC9IVRw").is_some());
        assert!(clean_url("https://youtube.com").is_none());
        assert!(clean_url("https://youtube.com/").is_none());
        assert!(clean_url("youtube.com/watch?v=1").is_none());
        assert!(clean_url("https://a b.com/x").is_none());
        assert!(clean_url("htt").is_none());
    }
}
