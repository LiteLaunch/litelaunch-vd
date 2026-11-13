//! vd — video downloader (yt-dlp), a LiteLaunch plugin (DLL). The link is checked and results
//! are cached here; Python (vd.py) runs only to extract a link and to download. vd.py speaks
//! Flow Launcher's JSON-RPC: `pythonw vd.py <request>`, the reply on stdout.
//! The script is embedded and unpacked to %APPDATA%\LiteLaunch\vd; yt-dlp lives in its lib.

use litelaunch_plugin::util::{CREATE_NEW_CONSOLE, CREATE_NO_WINDOW, clipboard_text, expand_env};
use litelaunch_plugin::{Action, After, Host, Info, Item, Plugin, Reply, export_plugin};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const FILES: &[(&str, &[u8])] = &[("vd.py", include_bytes!("vd.py")), ("icon.png", include_bytes!("icon.png")), ("vd.ico", include_bytes!("vd.ico"))];

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

#[derive(Serialize, Deserialize)]
pub enum Op {
    OpenFolder,
    InstallLib,
    /// A JSON-RPC action from vd.py's results (`download` with its parameters).
    Rpc { method: String, params: Value },
}

/// One extracted result, as vd.py gave it.
#[derive(Clone)]
struct Found {
    title: String,
    sub: String,
    icon: Option<String>,
    score: i64,
    rpc: Option<(String, Value)>,
}

#[derive(Default)]
pub struct Vd {
    /// The Python answering the current query, killed by `cancel`.
    child: Mutex<Option<Child>>,
    /// Extracted results per link.
    cache: Mutex<HashMap<String, (Instant, Vec<Found>)>>,
    /// A Python that has imported yt-dlp and waits for the request on stdin.
    warm: Mutex<Option<(Child, Instant)>>,
}

export_plugin!(Vd);

fn icon(host: &Host) -> String {
    host.data_dir.join("icon.png").to_string_lossy().into()
}

fn has_lib(host: &Host) -> bool {
    host.data_dir.join("lib").join("yt_dlp").exists()
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

fn request(method: &str, params: Value, settings: &VdConfig) -> String {
    json!({ "method": method, "parameters": params, "settings": settings }).to_string()
}

fn python(host: &Host) -> Command {
    let mut cmd = Command::new(&host.python);
    cmd.arg("-B").arg(host.data_dir.join("vd.py")).current_dir(&host.data_dir).env("PYTHONIOENCODING", "utf-8").creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// Case-insensitive field lookup: Flow accepts both `JsonRPCAction` and `jsonRPCAction` etc.
fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.as_object()?.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v)
}

fn parse(dir: &Path, r: &Value) -> Found {
    let s = |k| get(r, k).and_then(Value::as_str).unwrap_or_default().to_string();
    let icon_rel = s("IcoPath");
    let icon = [icon_rel.as_str(), "icon.png"].into_iter().find_map(|i| {
        if i.is_empty() || i.starts_with("http") {
            return None;
        }
        let path = dir.join(i.replace('/', "\\"));
        path.exists().then(|| path.to_string_lossy().into())
    });
    let rpc = get(r, "JsonRPCAction").filter(|a| a.is_object()).and_then(|a| {
        let method = get(a, "method").and_then(Value::as_str).unwrap_or_default().to_string();
        (!method.is_empty()).then(|| (method, get(a, "parameters").cloned().unwrap_or(json!([]))))
    });
    Found { title: s("Title"), sub: s("SubTitle"), icon, score: get(r, "Score").and_then(Value::as_i64).unwrap_or(0), rpc }
}

fn items(found: &[Found]) -> Vec<Item> {
    let mut v: Vec<Item> = found
        .iter()
        .map(|f| {
            let action = f.rpc.as_ref().map_or(Action::Nothing, |(method, params)| Action::op(&Op::Rpc { method: method.clone(), params: params.clone() }));
            Item::new(f.title.clone(), f.sub.clone(), f.icon.clone(), f.score, action)
        })
        .collect();
    v.sort_by(|a, b| b.score.cmp(&a.score));
    v
}

impl Vd {
    /// Starts a prewarmed Python unless one is already waiting.
    fn warm_up(&self, host: &Host) {
        let mut slot = self.warm.lock().unwrap();
        if slot.as_mut().is_some_and(|(c, at)| at.elapsed() < WARM_TTL && matches!(c.try_wait(), Ok(None))) {
            return;
        }
        *slot = python(host).arg("--wait").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().ok().map(|c| (c, Instant::now()));
    }

    /// The waiting Python, if it is still fresh enough to take a request.
    fn take_warm(&self) -> Option<Child> {
        let (mut c, at) = self.warm.lock().unwrap().take()?;
        if at.elapsed() < WARM_TTL && matches!(c.try_wait(), Ok(None)) { Some(c) } else { None }
    }

    /// Asks vd.py about a link; the child is parked so `cancel` can kill it mid-flight.
    fn extract(&self, host: &Host, cfg: &VdConfig, url: &str) -> Result<Vec<Found>, String> {
        let req = request("query", json!([url]), cfg);
        let warm = self.take_warm().and_then(|mut c| {
            let mut stdin = c.stdin.take()?;
            writeln!(stdin, "{req}").ok()?;
            Some(c)
        });
        let mut child = match warm {
            Some(c) => c,
            None => python(host)
                .arg(&req)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| format!("Не удалось запустить Python: {e}"))?,
        };
        let mut out = child.stdout.take().unwrap();
        let mut err = child.stderr.take().unwrap();
        *self.child.lock().unwrap() = Some(child);
        let err_reader = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = err.read_to_string(&mut s);
            s
        });
        let mut stdout = Vec::new();
        let _ = out.read_to_end(&mut stdout);
        let stderr = err_reader.join().unwrap_or_default();
        // Gone from the slot: `cancel` took and killed it.
        if self.child.lock().unwrap().take().and_then(|mut c| c.wait().ok()).is_none() {
            return Err("cancelled".into());
        }
        let stdout = String::from_utf8_lossy(&stdout);
        // Take the reply from the first brace: libraries sometimes print junk before it.
        let reply = stdout.find('{').and_then(|i| serde_json::from_str::<Value>(stdout[i..].trim()).ok());
        let Some(reply) = reply else {
            let msg = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("пустой ответ").trim().to_string();
            return Err(msg);
        };
        let results = get(&reply, "result").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(results.iter().map(|r| parse(&host.data_dir, r)).collect())
    }
}

impl Plugin for Vd {
    type Settings = VdConfig;
    type Op = Op;

    fn info(&self, host: &Host) -> Info {
        Info {
            name: "Video Downloader".into(),
            description: "Скачивает видео и звук по ссылке через yt-dlp: YouTube, VK, Twitch, TikTok и ещё ~1800 сайтов".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            icon: Some(icon(host)),
            ..Default::default()
        }
    }

    fn init(&self, host: &Host) {
        litelaunch_plugin::util::unpack(&host.data_dir, FILES);
    }

    fn query(&self, host: &Host, cfg: &VdConfig, q: &str) -> Reply {
        let item = |title: &str, sub: String, action: Action| Item::new(title, sub, Some(icon(host)), 0, action);
        if !host.python_ok {
            return vec![litelaunch_plugin::install_python_item(Some(icon(host)))].into();
        }
        if !has_lib(host) {
            return vec![item("Установить yt-dlp", "Enter запустит pip в отдельном окне".into(), Action::op(&Op::InstallLib))].into();
        }
        let folder = expand_env(&cfg.download_dir);
        let open_folder = item("Открыть папку загрузок", folder, Action::op(&Op::OpenFolder));
        let url = clean_url(q);
        if let Some(url) = &url {
            let cache = self.cache.lock().unwrap();
            if let Some((_, found)) = cache.get(url).filter(|(at, _)| at.elapsed() < CACHE_TTL) {
                return items(found).into();
            }
            return Reply { slow: Some(Value::String(url.clone())), slow_note: Some("Получаю форматы видео".into()), ..Default::default() };
        }
        // A link is probably coming: pay for the yt-dlp import now.
        if cfg.prewarm {
            self.warm_up(host);
        }
        if q.is_empty() {
            let first = match clean_url(&clipboard_text()) {
                Some(url) => item("Ссылка из буфера обмена", url.clone(), Action::SetQuery(format!("{} {url}", cfg.keyword))),
                None => item("Вставьте ссылку на видео", "YouTube, VK, Twitch, TikTok и ещё ~1800 сайтов".into(), Action::Nothing),
            };
            return vec![first, open_folder].into();
        }
        vec![item("Некорректная ссылка", "Ожидается ссылка вида https://сайт/видео".into(), Action::Nothing)].into()
    }

    fn query_slow(&self, host: &Host, cfg: &VdConfig, data: Value) -> Result<Vec<Item>, String> {
        let url = data.as_str().unwrap_or_default().to_string();
        let found = self.extract(host, cfg, &url)?;
        // Only real results: errors ("couldn't extract") are worth retrying.
        if found.iter().any(|f| f.rpc.is_some()) {
            let mut cache = self.cache.lock().unwrap();
            cache.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
            cache.insert(url, (Instant::now(), found.clone()));
        }
        Ok(items(&found))
    }

    fn cancel(&self) {
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
        }
    }

    fn run(&self, host: &Host, cfg: &mut VdConfig, op: Op) -> After {
        match op {
            Op::OpenFolder => {
                let folder = expand_env(&cfg.download_dir);
                let _ = std::fs::create_dir_all(&folder);
                let _ = Command::new("explorer.exe").arg(folder).spawn();
            }
            Op::InstallLib => {
                let python = litelaunch_plugin::util::console_python(&host.python);
                let lib = host.data_dir.join("lib");
                let _ = Command::new(python).args(["-m", "pip", "install", "--target"]).arg(lib).arg("yt-dlp[default]").creation_flags(CREATE_NEW_CONSOLE).spawn();
            }
            // Downloads take minutes: detached, vd.py reports through the launcher's tray icon.
            Op::Rpc { method, params } => {
                let _ = python(host).arg(request(&method, params, cfg)).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
            }
        }
        After::Hide
    }
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
