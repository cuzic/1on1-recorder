// spike-plan.md SPIKE-05: Tauri 2 常駐と録音ライフサイクル分離。
//
// ウィンドウを閉じてもトレイ常駐でバックエンド(ここではダミー音源による
// レベルメーター生成スレッド)が継続することを検証する。実際のWASAPI
// キャプチャ(SPIKE-01)の代わりに、経過時間から決定的に導出した疑似レベル値
// (乱数・システム時刻に依存しない)を30fpsで生成し続ける。

use serde::Serialize;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, WindowEvent};

const TICK_INTERVAL: Duration = Duration::from_millis(33); // 約30fps

#[derive(Debug, Clone, Copy, Serialize)]
struct LevelMeterTick {
    tick: u64,
    elapsed_ms: u64,
    rms: f32,
    peak: f32,
}

struct AppState {
    latest: Mutex<LevelMeterTick>,
}

/// 経過時間だけから決定的にレベル値を導出する(乱数を使わないので、
/// ソークテストの実行結果が再現可能になる)。
fn synthesize_level(elapsed: Duration) -> (f32, f32) {
    let t = elapsed.as_secs_f32();
    // ゆっくりした「発話っぽい」包絡線 + 短周期の揺らぎ。
    let envelope = 0.5 + 0.5 * (t * 0.3).sin();
    let jitter = 0.05 * (t * 13.7).sin() * (t * 3.1).cos();
    let rms = (envelope * 0.6 + jitter).clamp(0.0, 1.0);
    let peak = (rms + 0.15 * (t * 13.7).sin().abs()).clamp(0.0, 1.0);
    (rms, peak)
}

fn log_path() -> std::path::PathBuf {
    std::env::var("SPIKE05_LOG_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("level_meter_log.jsonl"))
}

/// バックグラウンドの「録音」スレッド。ウィンドウの表示/非表示に関わらず、
/// アプリが終了するまで動き続ける(design.mdの「ウィンドウ閉鎖後も録音が
/// 途切れない」という合否基準の中核)。
fn spawn_recording_thread(app: AppHandle, state: Arc<AppState>, visibility_toggle_requested: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let started_at = Instant::now();
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path())
            .expect("failed to open level meter log file");
        let mut tick_count: u64 = 0;

        loop {
            let elapsed = started_at.elapsed();
            let (rms, peak) = synthesize_level(elapsed);
            tick_count += 1;
            let tick = LevelMeterTick {
                tick: tick_count,
                elapsed_ms: elapsed.as_millis() as u64,
                rms,
                peak,
            };

            *state.latest.lock().unwrap() = tick;

            // ウィンドウが非表示でもemit自体は失敗しない(受け手がいないだけ)。
            // 「バックエンドが継続している」ことの独立した証拠として、
            // ウィンドウの状態に関わらずJSONLへも必ず記録する。
            let _ = app.emit("level-meter", tick);
            if let Ok(line) = serde_json::to_string(&tick) {
                let _ = writeln!(log, "{line}");
                let _ = log.flush();
            }

            if visibility_toggle_requested.swap(false, Ordering::SeqCst) {
                toggle_main_window_visibility(app.clone(), state.clone());
            }

            std::thread::sleep(TICK_INTERVAL);
        }
    });
}

/// SIGUSR1(Unix限定)や トレイメニュー「Show」から共通で呼ばれる、
/// メインウィンドウの表示/非表示切り替え。ウィンドウ操作はメインスレッドで
/// 行う必要があるため`run_on_main_thread`へ委譲する。
fn toggle_main_window_visibility(app: AppHandle, state: Arc<AppState>) {
    let _ = app.clone().run_on_main_thread(move || {
        let Some(window) = app.get_webview_window("main") else {
            return;
        };
        let currently_visible = window.is_visible().unwrap_or(true);
        if currently_visible {
            let _ = window.hide();
        } else {
            let _ = window.show();
            let _ = window.set_focus();
            // 再表示時に、次のtickを待たずその場の最新値をpushする
            // (WebViewが非表示中も生き続けているか、破棄され再生成されたか
            // に関わらず、復元表示を即座に行うため)。
            let snapshot = *state.latest.lock().unwrap();
            let _ = app.emit("level-meter-snapshot", snapshot);
        }
    });
}

#[cfg(unix)]
fn register_visibility_toggle_signal(flag: &Arc<AtomicBool>) {
    // ソークテストスクリプトが`kill -USR1 <pid>`でウィンドウの閉鎖/再表示を
    // 模擬できるようにする(Xvfb環境には実マウス操作がないため)。
    let _ = signal_hook::flag::register(signal_hook::consts::SIGUSR1, flag.clone());
}

#[cfg(not(unix))]
fn register_visibility_toggle_signal(_flag: &Arc<AtomicBool>) {}

#[tauri::command]
fn get_snapshot(state: tauri::State<'_, Arc<AppState>>) -> LevelMeterTick {
    *state.latest.lock().unwrap()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let state = Arc::new(AppState {
        latest: Mutex::new(LevelMeterTick {
            tick: 0,
            elapsed_ms: 0,
            rms: 0.0,
            peak: 0.0,
        }),
    });
    let visibility_toggle_requested = Arc::new(AtomicBool::new(false));
    register_visibility_toggle_signal(&visibility_toggle_requested);

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(state.clone())
        .invoke_handler(tauri::generate_handler![get_snapshot])
        .setup(move |app| {
            let app_handle = app.handle().clone();
            spawn_recording_thread(app_handle, state.clone(), visibility_toggle_requested.clone());

            let show_item = MenuItem::with_id(app, "show", "Show", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_item, &quit_item])?;

            let state_for_tray = state.clone();
            let _tray = TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "show" => toggle_main_window_visibility_to_shown(app.clone(), state_for_tray.clone()),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle().clone();
                        let state = app.state::<Arc<AppState>>().inner().clone();
                        toggle_main_window_visibility_to_shown(app, state);
                    }
                })
                .build(app)?;

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                // ウィンドウを実際には閉じさせず隠すだけにする。これにより
                // プロセス(と録音スレッド)がトレイ常駐のまま生き続ける。
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn toggle_main_window_visibility_to_shown(app: AppHandle, state: Arc<AppState>) {
    // トレイの「Show」クリック/左クリックは「常に表示する」という利用者の
    // 意図なので、非表示/表示のトグルではなく明示的にshowへ倒す。
    let _ = app.clone().run_on_main_thread(move || {
        let Some(window) = app.get_webview_window("main") else {
            return;
        };
        let _ = window.show();
        let _ = window.set_focus();
        let snapshot = *state.latest.lock().unwrap();
        let _ = app.emit("level-meter-snapshot", snapshot);
    });
}
