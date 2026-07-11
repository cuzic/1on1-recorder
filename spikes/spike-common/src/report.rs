// 「起動してボタン(Enter)を押したら検証が終わり、コンソールの内容を
// コピペすればログと合否が分かる」という運用を、SPIKE-01/02/03/11の
// バイナリ間で共通のフォーマットで提供するためのヘルパー。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Ctrl+C(Windowsでは Ctrl+Break も)を捕捉し、フラグを立てるだけにする。
/// 録音ループ側は`duration_secs`経過だけでなくこのフラグも見て早期終了し、
/// 通常終了と同じ経路(集計→summary.json書き込み→結果レポート表示→Enter待ち)を
/// 必ず通るようにする。「Ctrl+Cで終了できます」と案内しておきながら、実際には
/// 何も出力されずに終わってしまう問題を避けるため。
///
/// ハンドラ登録に失敗した場合(二重登録など)でも録音自体は継続できるよう、
/// 呼び出し側にpanicさせずフラグだけ返す(常にfalseから始まる)。
pub fn install_ctrlc_stop_flag() -> Arc<AtomicBool> {
    let stop_requested = Arc::new(AtomicBool::new(false));
    let flag = stop_requested.clone();
    if let Err(e) = ctrlc::set_handler(move || {
        flag.store(true, Ordering::SeqCst);
    }) {
        eprintln!("警告: Ctrl+Cハンドラの登録に失敗しました({e})。Ctrl+Cで終了すると結果レポートが表示されない可能性があります。");
    }
    stop_requested
}

/// 実行開始時に、これから何秒間・何をすればよいかを画面へ大きく表示する。
pub fn print_banner(spike_id: &str, title: &str, instructions: &[&str]) {
    println!();
    println!("======================================================================");
    println!(" {spike_id}: {title}");
    println!("======================================================================");
    for line in instructions {
        println!(" {line}");
    }
    println!("======================================================================");
    println!();
}

/// `acceptance`オブジェクトの各フィールドを、真偽値なら[PASS]/[FAIL]、
/// nullなら[MANUAL](自動判定不可・目視確認が必要)、それ以外は[INFO]として
/// 1行ずつ表示する。末尾に自動判定できた項目の合格数もまとめて出す。
pub fn print_acceptance_report(spike_id: &str, title: &str, acceptance: &serde_json::Value) {
    println!();
    println!("======================================================================");
    println!(" {spike_id}: {title} — 結果サマリ(この節から下をコピペしてください)");
    println!("======================================================================");

    let mut pass = 0u32;
    let mut fail = 0u32;
    let mut manual = 0u32;

    if let Some(obj) = acceptance.as_object() {
        for (key, value) in obj {
            let (tag, is_bool) = match value {
                serde_json::Value::Bool(true) => ("[PASS]", true),
                serde_json::Value::Bool(false) => ("[FAIL]", true),
                serde_json::Value::Null => ("[MANUAL]", false),
                _ => ("[INFO]", false),
            };
            match tag {
                "[PASS]" => pass += 1,
                "[FAIL]" => fail += 1,
                "[MANUAL]" => manual += 1,
                _ => {}
            }
            let _ = is_bool;
            println!("{tag:9} {key} = {value}");
        }
    } else {
        println!("(acceptanceオブジェクトが空、または想定外の形式です)");
    }

    println!("----------------------------------------------------------------------");
    println!(
        "自動判定: PASS={pass} FAIL={fail}  (MANUAL={manual}件は目視・実機での確認が必要)"
    );
    println!("======================================================================");
    println!();
    println!("--- 詳細ログ(acceptance全体のJSON) ---");
    if let Ok(text) = serde_json::to_string_pretty(acceptance) {
        println!("{text}");
    }
    println!();
}

/// 実行終了時にコンソールが即座に閉じてしまわないよう、Enterキー入力を待つ。
/// (エクスプローラーからの直接ダブルクリック起動を想定)
pub fn pause_before_exit() {
    println!("Enterキーを押すとこのウィンドウを閉じます...");
    let mut buf = String::new();
    let _ = std::io::stdin().read_line(&mut buf);
}
