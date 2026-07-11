# Windowsビルド・実行検証の状況

* **文書ステータス**: Draft v0.6
* **作成日**: 2026-07-09
* **更新(2026-07-11)**: ユーザーが実際にWindows実機(Bluetoothヘッドセット接続状態)でSPIKE-01/02/11の`.exe`を実行し、初めての実機実行データを取得した。詳細は§4「B区分スパイクの実機再現ログ」参照。この実行で(a) Ctrl+Cで早期終了すると集計・レポートが一切生成されないUXバグ、(b) SPIKE-11で荒れたBluetoothエンドポイントが後続イベント処理を数十秒遅延させる設計上の弱点、の2件を発見しどちらも修正済み(クロスコンパイル型検証のみ、実機での再検証は未実施)。

---

## 1. ビルド確認はLinux側で完了済み(Windows実機不要)

`spikes/`ワークスペースは、Windows実機を使わずLinux上の`x86_64-w64-mingw32-gcc`(mingw-w64)クロスコンパイル環境で以下まで確認済み。

```bash
rustup target add x86_64-pc-windows-gnu   # 済み
cd spikes
cargo build --workspace --target x86_64-pc-windows-gnu           # debug: 成功
cargo build --workspace --release --target x86_64-pc-windows-gnu # release: 成功
cargo test --workspace --target x86_64-pc-windows-gnu --no-run   # テストハーネスのビルド: 成功
```

* 生成物は`file`コマンドで`PE32+ executable (console) x86-64, for MS Windows`と確認済み(本物のWindows実行ファイル)
* `spike-common`・`spike-01-wasapi-dual-capture`・`spike-02-app-loopback`の3クレートすべてがエラー・警告なくコンパイル・リンクできる(`cargo build`/`--release`/`cargo test --no-run`のいずれもクリーン)
* ワークスペース全体で`todo!()`は0件(`grep -rn "todo!(" spike-*/src/`で確認)
* `spike-common`に実装した主なもの:
  * `run_capture_loop`/`copy_to_f32_vec`/`wait_for_multiple`等をspike-01の`wasapi_common.rs`から抽出し、SPIKE-02とも共有する構成にした(§10推奨実装順序 手順6)
  * `aggregator.rs`(`Aggregator`/`StreamCaptureResult`)も同様にSPIKE-01から抽出し、任意の`(StreamId, ファイル名)`の組を受け取れるよう一般化してSPIKE-02からも再利用できるようにした(§5.2)
  * `device_watch.rs`: `IMMNotificationClient`実装(spike-windows-11-detail-design.md §3-4の設計を、SPIKE-09が要求する最小限のイベント観測だけに絞って先行実装。SPIKE-11本体のスナップショット管理は別途)
  * `capture_loop.rs`に`IAudioSessionEvents::OnSessionDisconnected`の観測と`AUDCLNT_E_DEVICE_INVALIDATED`の検出→`CaptureExit::DeviceLost`変換を追加(spike-plan.md SPIKE-09)
* `spike-01-wasapi-dual-capture`に実装した主なもの: `CoInitializeEx`/`CoUninitialize`、`StopSignal`、`AudioFormatInfo::from_waveformatex`、MMCSS、`RtlGetVersion`(ntdll直リンク)、`QueryPerformanceCounter`/`Frequency`、`IMMDeviceEnumerator`列挙・解決、`summary.json`構築、および**SPIKE-09相当の自動復帰ループ**(`StreamSupervisor`がデバイス消失を検出したら`device_events.jsonl`へ記録し、同じdevice_id_or_defaultで再解決・再初期化を試みる。上限は`--max-recovery-attempts`)
* `spike-02-app-loopback`に実装した主なもの: `AUDIOCLIENT_ACTIVATION_PARAMS`のPROPVARIANT(VT_BLOB)化と`ActivateAudioInterfaceAsync`呼び出し、`build_fixed_format_48k_stereo_f32`、診断用ハードタイムアウト経路の`park_pending_activation`、`process_events.jsonl`/`summary.json`出力
* `spike-11-endpoint-registry`(新規クレート)に実装した主なもの: 起動時の全endpoint列挙(`Active`/`Disabled`/`NotPresent`/`Unplugged`すべて)による初期`AudioEndpointSnapshot`レジストリ構築、`spike-common::device_watch`が送る生イベントを受けて該当endpointを`IPropertyStore`/`IAudioEndpointVolume`経由で再取得・更新する`EndpointRegistry`、既定ルート(6通りのflow×role)の追跡、`endpoint_events.jsonl`/`registry_snapshot.json`/`summary.json`出力

**したがって「ビルドが通るか」を確認するためだけにWindows実機・clipwire-exec等を使う必要はない。** ネイティブ(MSVCまたはWindows上のmingw)とクロスコンパイル(Linux上のmingw-w64)でリンカ実装が異なるため理論上は差が出る余地はあるが、`windows`クレート自体がクロスプラットフォームでの一貫性を前提に設計されているため、優先度は低い。

---

## 2. Windows実機でしか確認できないこと

以下は原理的にLinux側では確認できず、Windows実機が必要。

1. **OSビルド番号**(design.md §5.1 / spike-windows-01-02-detail-design.md §7-6: Process Loopback対応の最低ビルドがWindows 10 build 20348以降、情報源によっては20438以降)
   ```powershell
   winver
   # または
   [System.Environment]::OSVersion.VersionString
   Get-ItemProperty 'HKLM:/SOFTWARE/Microsoft/Windows NT/CurrentVersion' | Select-Object CurrentBuild, UBR, DisplayVersion
   ```
   (レジストリパスは`\`ではなく`/`区切りで書く方が、PowerShell文字列とスクリプトのエスケープが重ならず安全)

2. **実際の`.exe`実行結果** — 2026-07-11にSPIKE-01/02/11の実機実行を確認済み(§4参照)。SPIKE-01/02はCtrl+Cによる早期終了時にsummary.jsonが生成されないバグがあったため修正版での10分フル実行の再確認が必要。

3. **WASAPIマイク/Endpoint Loopbackの同時取得**(SPIKE-01本来の検証)— 約162秒間の部分的な実機データは取得済み(discontinuity 1件、質は良好)。10分フルでの正式な合否判定は未実施。

4. **Application Loopback Captureの実際の動作**(SPIKE-02本来の検証)— `ActivateAudioInterfaceAsync`のRust `windows` crateからの呼び出し可否という最大の不確実点は**実機で解消済み**(クラッシュせず完了ハンドラが起動しキャプチャ成立)。「対象アプリの音声のみを分離取得でき他アプリ音声が混入しない」という合否基準は、対象アプリを継続再生させた状態での再テストが必要。

5. **`IActivateAudioInterfaceCompletionHandler`・`IMMNotificationClient`の実際のコールバック挙動** — 実機で正常に動作することを確認済み(SPIKE-11で145件のPropertyValueChangedコールバックを実地観測)。

6. **spike-plan.md SPIKE-09相当の自動復帰が実際に機能するか** — 2026-07-11の実機実行ではUSB抜き差し等は行われなかった(Bluetoothヘッドセット接続のみ)。デバイス消失検出→`StreamSupervisor`による再アタッチが実際に機能するかは、USBマイク抜去・既定デバイス切替・スリープ復帰を伴う再テストが必要。

---

## 3. 次のステップ

SPIKE-01・SPIKE-02・SPIKE-09・SPIKE-11相当は実装が完了したため、`spike-windows-01-02-detail-design.md` §4.11(SPIKE-01実行手順)・§5.10(SPIKE-02実行手順)・`spike-windows-11-detail-design.md` §9(SPIKE-11実行手順)に従ってWindows実機で検証できる状態にある。ただし、実機での実行検証(マイク/スピーカーへ向けて実行しCSV/WAV/summary.jsonを確認する作業、Zoom/Teamsでの実会議音声取得、実際のデバイス抜き差し、USBマイク/ヘッドセットの抜き差しやWindows設定でのデバイス無効化・既定デバイス変更)は、方針どおり**開発がある程度まとまった段階でまとめて行う**(このタイミングでは実施しない)。

実機作業が必要になった段階では、「git pull → 実装追加分のビルド → 実際にマイク/スピーカーへ向けて実行、USBマイクの抜き差し・スリープ・BT切替・Zoom会議参加 → 各jsonl/summary.jsonを確認」という、ビルドだけでなく実データ・実操作を扱う内容になるため、その時点で改めて実機作業の方法(手動 / clipwire-exec)を判断する。

---

## 4. B区分スパイクの実機再現ログ

* 2026-07-10: SPIKE-03(共通タイムライン整列・drift補正)をWindows実機で実行し、`spikes/spike-03-timeline-drift/out/full-2h/summary.json`(コミット済み)と同一のacceptance結果(4項目すべてPASS)を確認。

* 2026-07-11: SPIKE-01/02/11をWindows実機(Bluetoothヘッドセット接続状態)で初めて実行。
  * **SPIKE-11**: 90秒間完走しsummary.json生成まで到達。`apply_errors=0`・`no_duplicate_registration=true`・`no_leaked_registration=true`など自動判定項目は全PASS。一方、切断済み("NotPresent")のBluetoothマイクendpointから約61秒間に145件の`PropertyValueChanged`が連続発生し、1件ずつ同期的なCOM再照会が積み重なって`dispatch_latency_us`が平均31.5秒・最大57.4秒まで悪化する事例を実地で発見。`ENDPOINT_REFRESH_TIMEOUT`(1.5秒)による打ち切りを実装し修正(§spike-plan.md SPIKE-11参照。実機での再検証は未実施)。
  * **SPIKE-01**: 既定600秒を待たずCtrl+Cで約162秒時点で終了。Ctrl+Cが集計・summary.json書き込み・結果レポート表示のコードを一切経由せずプロセスを終了させるバグが見つかり修正(`spike_common::report::install_ctrlc_stop_flag`)。生CSVを手動集計した限りではdiscontinuity 1件・timestamp_error 0件・silent 0件、フレームレート約48,013Hzとデータの質自体は良好だった。
  * **SPIKE-02**: `ActivateAudioInterfaceAsync`をRustから呼び出せること自体を確認(クラッシュせず完了ハンドラが起動、キャプチャ成立)。音声データを含む行は約6.7秒分のみ(対象アプリがほとんどの時間無音だったためと推定、無音区間はコールバックが来ない仕様どおり)。SPIKE-01と同じCtrl+C要因でsummary.json未生成。
  * 上記2件の修正版(Ctrl+C対応、SPIKE-11のタイムアウト対応)は、この環境でのクロスコンパイル型検証(`cargo build --release --target x86_64-pc-windows-gnu`)のみ完了しており、Windows実機での再実行はまだ行っていない。
