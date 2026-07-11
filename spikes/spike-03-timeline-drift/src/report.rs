// 「起動してEnterを押すだけで検証が終わり、コンソールの内容をコピペすれば
// ログと合否が分かる」という運用のための表示ヘルパー。
//
// spike-commonにも同じ内容のモジュールがあるが、spike-commonはwindowsクレートに
// 依存しておりWindows(またはそのクロスコンパイル)でしかビルドできない。
// 本クレートはOS非依存でこのLinux環境でも`cargo test`まで完結させる設計
// (Cargo.tomlのコメント参照)のため、あえて共有せずこの小さなモジュールだけ
// ローカルに複製する。

/// 実行開始時に、これから何をするか・どれくらい待つかを画面へ大きく表示する。
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
            let tag = match value {
                serde_json::Value::Bool(true) => "[PASS]",
                serde_json::Value::Bool(false) => "[FAIL]",
                serde_json::Value::Null => "[MANUAL]",
                _ => "[INFO]",
            };
            match tag {
                "[PASS]" => pass += 1,
                "[FAIL]" => fail += 1,
                "[MANUAL]" => manual += 1,
                _ => {}
            }
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
