# Windowsビルド・実行検証の状況

* **文書ステータス**: Draft v0.4
* **作成日**: 2026-07-09
* **更新**: ビルド確認はLinux上の`mingw-w64`クロスコンパイルで完結することが分かったため、Windows実機での作業は「実機でしか確認できないこと」だけに絞った。SPIKE-01・SPIKE-02・SPIKE-09相当(デバイス変更観測+最小限の自動復帰)の`todo!()`は全て実装済み。実機での実行検証は、方針どおり開発がまとまった段階でまとめて行う。

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

2. **実際の`.exe`実行結果** — SPIKE-01・SPIKE-02とも`todo!()`を全て実装済みのため、Windows実機であれば両方とも意味のある結果(CSV/WAV/summary.json、SPIKE-02は加えてprocess_events.jsonl)が得られる状態になった。

3. **WASAPIマイク/Endpoint Loopbackの同時取得**(SPIKE-01本来の検証。実装は完了、実機実行はまだ)

4. **Application Loopback Captureの実際の動作**(SPIKE-02本来の検証。実装は完了、実機実行はまだ。特に`ActivateAudioInterfaceAsync`のRust `windows` crateからの呼び出し可否そのものが最大の不確実点で、これはコンパイルが通ることでは確認できない)

5. **`IActivateAudioInterfaceCompletionHandler`・`IMMNotificationClient`の実際のコールバック挙動**(実装は完了、実機実行はまだ)

6. **spike-plan.md SPIKE-09相当の自動復帰が実際に機能するか** — USBマイク抜去・既定デバイス切替・スリープ復帰・Bluetooth切替を実際に行い、`device_events.jsonl`に記録されるか、`StreamSupervisor`が再アタッチしてストリームが復帰するかは実機でしか確認できない

---

## 3. 次のステップ

SPIKE-01・SPIKE-02・SPIKE-09相当は実装が完了したため、`spike-windows-01-02-detail-design.md` §4.11(SPIKE-01実行手順)・§5.10(SPIKE-02実行手順)に従ってWindows実機で検証できる状態にある。ただし、実機での実行検証(マイク/スピーカーへ向けて実行しCSV/WAV/summary.jsonを確認する作業、Zoom/Teamsでの実会議音声取得、実際のデバイス抜き差し)は、方針どおり**開発がある程度まとまった段階でまとめて行う**(このタイミングでは実施しない)。

SPIKE-11(Audio Endpoint Registry本体、AudioEndpointSnapshotのレジストリ管理)は、`spike-windows-11-detail-design.md`に沿って別途実装が残っている(今回のdevice_watch.rsはSPIKE-09が要求する最小限のイベント観測のみで、SPIKE-11本体のスナップショット管理・初期列挙は含まない)。

実機作業が必要になった段階では、「git pull → 実装追加分のビルド → 実際にマイク/スピーカーへ向けて実行、USBマイクの抜き差し・スリープ・BT切替・Zoom会議参加 → 各jsonl/summary.jsonを確認」という、ビルドだけでなく実データ・実操作を扱う内容になるため、その時点で改めて実機作業の方法(手動 / clipwire-exec)を判断する。
