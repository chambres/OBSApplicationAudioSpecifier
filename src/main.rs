// Temporarily disable windows_subsystem so running from a console shows logs while debugging.
// Re-enable (remove the comment) for a GUI-only build when done.
// #![windows_subsystem = "windows"]

use std::process::{Child, Command, Stdio};
use anyhow::{anyhow, Context, Result};
use std::sync::{Arc, Mutex};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    io::Cursor,
    os::windows::ffi::OsStringExt,
    path::Path,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tray_icon::{
    icon::Icon as TrayIconImage,
    menu::{Menu, MenuEvent, MenuItem},
    TrayIconBuilder,
};
use windows::Win32::{
    Foundation::{HWND, WPARAM, LPARAM},
    System::{
        ProcessStatus::K32GetProcessImageFileNameW,
        Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, GetCurrentThreadId},
    },
    UI::{
        Input::KeyboardAndMouse::GetAsyncKeyState,
        WindowsAndMessaging::{
            DispatchMessageW, GetForegroundWindow, GetMessageW, GetWindowTextW,
            GetWindowThreadProcessId, TranslateMessage, MSG, PostThreadMessageW, WM_QUIT,
        },
    },
};

// ======= Config =======
const OBS_HOST: &str = "localhost";
const OBS_PORT: u16 = 4455;
const OBS_PASSWORD: &str = "D9IoZgkKlJOD9nFj"; // change if needed
const TARGET_INPUT_NAME: &str = "Application Audio Capture (BETA)";

// Keys
const VK_SHIFT: i32 = 0x10;
const VK_CONTROL: i32 = 0x11;
const VK_OEM_1: i32 = 0xBA; // ;:

fn main() -> Result<()> {
    // ---- Launch system OBS and monitor it ----
    let obs_child = launch_obs()?;

    // Wrap child in Arc<Mutex<>> so multiple threads can kill/wait on it.
    let obs_child = Arc::new(Mutex::new(obs_child));

    // Channel to stop the worker thread
    let (tx_quit, rx_quit) = mpsc::channel::<()>();
    let (tx_obs_exit, rx_obs_exit) = mpsc::channel::<()>();

    // Watcher thread: poll the child, send notification when it exits
    let main_thread_id = unsafe { GetCurrentThreadId() };
    {
        let tx_obs_exit = tx_obs_exit.clone();
        let obs_child = Arc::clone(&obs_child);
        thread::spawn(move || {
            loop {
                // Check if child exited
                let mut guard = match obs_child.lock() {
                    Ok(g) => g,
                    Err(_) => break,
                };
                match guard.try_wait() {
                    Ok(Some(_status)) => {
                        let _ = tx_obs_exit.send(());
                        // Wake the main thread message loop so it can observe rx_obs_exit.
                        unsafe {
                            let _ = PostThreadMessageW(main_thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
                        }
                        break;
                    }
                    Ok(None) => {
                        // still running
                    }
                    Err(_) => {
                        let _ = tx_obs_exit.send(());
                        unsafe {
                            let _ = PostThreadMessageW(main_thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
                        }
                        break;
                    }
                }
                drop(guard);
                thread::sleep(Duration::from_millis(250));
            }
        });
    }

    // ---- Build tray icon + menu ----
    let tray_menu = Menu::new();
    let quit_item = MenuItem::new("Quit", true, None);
    tray_menu.append(&quit_item)?;
    let mut builder = TrayIconBuilder::new()
        .with_tooltip("OBS Hotkey (Ctrl+Shift+;)")
        .with_menu(Box::new(tray_menu));

    if let Ok(icon) = load_tray_icon_from_ico(include_bytes!("icon.ico")) {
        builder = builder.with_icon(icon);
    }
    let _tray = builder.build()?;

    // ---- Spawn background worker ----
    // Worker thread (hotkey poll) uses tx_quit and rx_quit
    {
        let worker_rx_quit = rx_quit; // move receiver into worker
        thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        rt.block_on(async move {
            let mut last_fire = Instant::now() - Duration::from_secs(2);
            loop {
                if worker_rx_quit.try_recv().is_ok() {
                    break;
                }

                // Ctrl + Shift + ;
                if is_key_down(VK_CONTROL) && is_key_down(VK_SHIFT) && is_key_down(VK_OEM_1) {
                    if last_fire.elapsed() > Duration::from_millis(700) {
                        last_fire = Instant::now();
                        if let Ok(Some((exe_name, title, fallback))) = get_focus_components_for_obs() {
                            if let Err(e) = smart_update_obs_window(
                                TARGET_INPUT_NAME,
                                &exe_name,
                                &title,
                                &fallback,
                            )
                            .await
                            {
                                eprintln!("OBS update failed: {e:#}");
                            }
                        }
                    }
                }

                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        });
        });
    }

    // Register Ctrl-C handler: kill OBS and exit
    {
        let obs_child = Arc::clone(&obs_child);
        let tx_quit = tx_quit.clone();
        ctrlc::set_handler(move || {
            // Try to kill OBS
            if let Ok(mut guard) = obs_child.lock() {
                let _ = guard.kill();
            }
            let _ = tx_quit.send(());
        })?;
    }

    // ---- Windows message loop ----
    unsafe {
        let mut msg = MSG::default();
        loop {
            if GetMessageW(&mut msg, HWND(std::ptr::null_mut()), 0, 0).0 == 0 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);

            // Tray quit
            if let Ok(ev) = MenuEvent::receiver().try_recv() {
                if ev.id == quit_item.id() {
                    // Kill OBS and signal quit
                    if let Ok(mut guard) = obs_child.lock() {
                        let _ = guard.kill();
                    }
                    let _ = tx_quit.send(());
                    break;
                }
            }

            // OBS exited
            if rx_obs_exit.try_recv().is_ok() {
                // signal quit so worker thread and main will exit
                let _ = tx_quit.send(());
                break;
            }

            std::thread::sleep(Duration::from_millis(50));
        }
    }

    Ok(())
}

// ================= Helpers =================

fn launch_obs() -> Result<Child> {
    let obs_path = r"C:\Program Files\obs-studio\bin\64bit\obs64.exe";

    if !Path::new(obs_path).exists() {
        return Err(anyhow!("OBS not found at {}", obs_path));
    }

    // Launch OBS with its install directory as the current directory so it can find
    // locale and resource files. Inherit stdout/stderr so logs/errors are visible
    // when running from a console (helpful for diagnosing missing locale/ico errors).
    let child = Command::new(obs_path)
        // Use the obs64.exe binary folder as the current directory so OBS finds
        // its plugins, locales and other runtime resources.
        .current_dir(Path::new(r"C:\Program Files\obs-studio\bin\64bit"))
        .arg("--disable-shutdown-check")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("Failed to launch {}", obs_path))?;

    Ok(child)
}


fn load_tray_icon_from_ico(bytes: &[u8]) -> Result<TrayIconImage> {
    let dir = ico::IconDir::read(Cursor::new(bytes)).context("reading icon.ico")?;
    let entry = dir
        .entries()
        .iter()
        .max_by_key(|e| (e.width() as u32) * (e.height() as u32))
        .ok_or_else(|| anyhow!("icon.ico has no entries"))?;
    let image = entry.decode().context("decode icon frame")?;
    let (w, h) = (image.width(), image.height());
    TrayIconImage::from_rgba(image.rgba_data().to_vec(), w, h).context("create tray icon")
}

fn is_key_down(vk: i32) -> bool {
    unsafe { (GetAsyncKeyState(vk) as u16) & 0x8000 != 0 }
}

/// Return (exe_name, title, fallback_value_for_old_format)
fn get_focus_components_for_obs() -> Result<Option<(String, String, String)>> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return Ok(None);
        }

        let title = get_window_text(hwnd);
        if title.trim().is_empty() {
            return Ok(None);
        }

        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return Ok(None);
        }

        let exe_path = get_process_image_path(pid)?;
        let exe_name = Path::new(&exe_path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown.exe")
            .to_string();
        let app_name = exe_name.trim_end_matches(".exe").to_string();

        // Old fallback in case enumeration fails
        let fallback_value = format!("{app_name}:{title}:{exe_name}");
        Ok(Some((exe_name, title, fallback_value)))
    }
}

/// Ask OBS for the actual "window" list items, choose the best for the exe+title, then set it.
async fn smart_update_obs_window(
    input_name: &str,
    exe_name: &str,
    title: &str,
    fallback_value: &str,
) -> Result<()> {
    let uri = format!("ws://{}:{}/", OBS_HOST, OBS_PORT);
    let (ws_stream, _) = connect_async(&uri).await.context("connect OBS WebSocket")?;
    let (mut write, mut read) = ws_stream.split();

    // ----- Hello -----
    let hello_msg = read
        .next()
        .await
        .ok_or_else(|| anyhow!("OBS closed connection before Hello"))??;
    let hello_val: Value = serde_json::from_str(hello_msg.to_text()?)?;

    // ----- Identify (auth if required) -----
    let mut identify = json!({"op": 1, "d": {"rpcVersion": 1}});
    if let Some(auth) = hello_val["d"]["authentication"].as_object() {
        let challenge = auth["challenge"].as_str().unwrap_or("");
        let salt = auth["salt"].as_str().unwrap_or("");
        let secret_b64 = B64.encode(Sha256::digest(format!("{OBS_PASSWORD}{salt}").as_bytes()));
        let auth_b64 = B64.encode(Sha256::digest(format!("{secret_b64}{challenge}").as_bytes()));
        identify["d"]["authentication"] = Value::String(auth_b64);
    }
    write.send(Message::Text(identify.to_string())).await?;
    let _ = read.next().await;

    // ----- Enumerate valid "window" dropdown items -----
    let list_req = json!({
        "op": 6,
        "d": {
            "requestType": "GetInputPropertiesListPropertyItems",
            "requestId": "list-window-items",
            "requestData": {
                "inputName": input_name,
                "propertyName": "window"
            }
        }
    });
    write.send(Message::Text(list_req.to_string())).await?;
    let list_resp = read.next().await.ok_or_else(|| anyhow!("No list response"))??;
    let list_val: Value = serde_json::from_str(list_resp.to_text()?)?;

    let items: Vec<String> = list_val["d"]["responseData"]["items"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|it| it.get("value").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .collect();

    // Items typically look like: "[Spotify.exe]: Spotify" or "[Spotify.exe]: Drake - Mob Ties"
    let exe_bracket = format!("[{}]", exe_name);
    let exe_prefix = format!("{}:", exe_bracket); // "[Spotify.exe]:"

    let mut candidates: Vec<&str> = items
        .iter()
        .filter(|s| s.starts_with(&exe_prefix))
        .map(|s| s.as_str())
        .collect();

    // Choose the best candidate: prefer one that contains the current title (case-insensitive)
    let chosen = if candidates.is_empty() {
        // fallback to old constructed value if list is empty or exe not found
        fallback_value.to_string()
    } else {
        let title_l = title.to_lowercase();
        // Search for one containing the title; otherwise choose the longest
        candidates.sort_by_key(|s| s.len());
        let by_title = candidates
            .iter()
            .rev()
            .find(|s| s.to_lowercase().contains(&title_l));
        by_title
            .copied()
            .unwrap_or_else(|| *candidates.last().unwrap())
            .to_string()
    };

    // ----- Apply it -----
    let set_req = json!({
        "op": 6,
        "d": {
            "requestType": "SetInputSettings",
            "requestId": "setAppAudio",
            "requestData": {
                "inputName": input_name,
                "inputSettings": { "window": chosen },
                "overlay": true
            }
        }
    });
    write.send(Message::Text(set_req.to_string())).await?;
    let _ = read.next().await;

    Ok(())
}

unsafe fn get_window_text(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let len = GetWindowTextW(hwnd, &mut buf);
    OsString::from_wide(&buf[..len as usize])
        .to_string_lossy()
        .into_owned()
}

fn get_process_image_path(pid: u32) -> Result<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
            .ok()
            .ok_or_else(|| anyhow!("OpenProcess failed for PID {}", pid))?;
        let mut buf = vec![0u16; 32768];
        let len = K32GetProcessImageFileNameW(handle, &mut buf) as usize;
        let _ = windows::Win32::Foundation::CloseHandle(handle);
        if len == 0 {
            return Err(anyhow!("K32GetProcessImageFileNameW failed"));
        }
        Ok(String::from_utf16_lossy(&buf[..len]))
    }
}
