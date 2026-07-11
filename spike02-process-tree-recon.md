# 会議アプリのプロセスツリー実地観察手順(SPIKE-02準備)

* **文書ステータス**: Draft v0.1
* **作成日**: 2026-07-10
* **目的**: SPIKE-02(Application Loopback Capture)の対象プロセス選択(`ProcessSelectionStrategy`、spike-windows-01-02-detail-design.md §5.3)が、実際のZoom/Teams/Chromeのプロセス構造に対して機能するかを、Rustコードを書く前に手早く確認する。**コード不要、Windows実機とPowerShellだけで完結する**。

---

## 1. 観察の考え方

Windows Process Loopback(`AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK` + `PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE`)は、対象プロセスと**その子孫プロセス全体**(起動後に新規生成されるものも含めて動的に追従)を対象にする。したがって、

* 「実際に音を出している正確なPID」を当てる必要はない
* 「その音声プロセスの祖先である**根(root)プロセス**を正しく選べるか」だけが論点になる

本観察では、(a) 待機時のプロセスツリー形状、(b) 会議参加・画面共有時に新規生成されるプロセスの親子関係、を記録し、`ProcessSelectionStrategy::Root`(§5.3)のヒューリスティックが実際に妥当な根を選べているかを検証する。

---

## 2. スナップショット取得コマンド(PowerShell)

基本のワンライナー:

```powershell
Get-CimInstance Win32_Process |
  Where-Object { $_.Name -match 'zoom|teams|ms-teams|chrome' } |
  Select-Object ProcessId, ParentProcessId, Name, CreationDate, CommandLine |
  Sort-Object ProcessId |
  Format-Table -AutoSize -Wrap
```

ファイルへ保存し、後でタイミング間の差分を取れるようにする版:

```powershell
$snap = Get-CimInstance Win32_Process |
  Where-Object { $_.Name -match 'zoom|teams|ms-teams|chrome' } |
  Select-Object ProcessId, ParentProcessId, Name, CreationDate, CommandLine
$snap | Export-Csv -NoTypeInformation -Path "$env:USERPROFILE/proctree_$(Get-Date -Format yyyyMMdd_HHmmss).csv"
$snap | Format-Table -AutoSize -Wrap
```

(パス区切りは`\`ではなく`/`。バックスラッシュはPowerShell文字列・別ツール連携時のエスケープが重なって壊れやすいことが直近の作業で分かっているため。)

---

## 3. ツリー表示スクリプト(見やすさ用、任意)

親子関係をインデント付きツリーとして表示し、あわせて「根候補」を自動抽出する。抽出ロジックは`ProcessSelectionStrategy::Root`(親PIDが同名プロセス群に含まれないものを根とみなす)をそのままPowerShellへ落としたもの — **この観察自体が設計ヒューリスティックの妥当性を検証する材料になる**。

```powershell
function Show-ProcessTree {
    param($Processes, $ParentId = 0, $Indent = 0)
    $children = $Processes | Where-Object { $_.ParentProcessId -eq $ParentId }
    foreach ($c in $children) {
        "{0}{1} (PID={2}, PPID={3})" -f (' ' * $Indent), $c.Name, $c.ProcessId, $c.ParentProcessId
        Show-ProcessTree -Processes $Processes -ParentId $c.ProcessId -Indent ($Indent + 2)
    }
}

$all = Get-CimInstance Win32_Process | Select-Object ProcessId, ParentProcessId, Name
$target = $all | Where-Object { $_.Name -match 'zoom|teams|chrome' }
$rootCandidates = $target | Where-Object {
    $parent = $all | Where-Object ProcessId -eq $_.ParentProcessId
    -not ($parent -and $parent.Name -match 'zoom|teams|chrome')
}

foreach ($root in $rootCandidates) {
    "=== root candidate: $($root.Name) (PID=$($root.ProcessId), PPID=$($root.ParentProcessId)) ==="
    Show-ProcessTree -Processes $all -ParentId $root.ProcessId -Indent 2
}
```

---

## 3.5 自動化版: `watch-process-tree.ps1`

§2〜3の手動スナップショットは、会議参加・画面共有開始などの**タイミングに合わせて手動でPowerShellを叩く**必要があり、狙ったタイミングを逃しやすい。[watch-process-tree.ps1](watch-process-tree.ps1)は同じロジックをポーリングループ化し、実行して放置するだけでプロセスの生成/消滅イベントをタイムスタンプ付きJSONLへ自動記録する。

```powershell
# 既定(2秒間隔、zoom|teams|ms-teams|chromeを対象)で起動し、放置する
.\watch-process-tree.ps1

# この間にZoomへサインイン → テスト会議参加 → 画面共有 → 退出 → 終了、を普通に行うだけでよい
# Ctrl+C で停止すると、$PWD に proctree_watch_YYYYMMDD_HHMMSS.jsonl が残る
```

`process_added`イベントには`root_candidate`(親PIDが対象プロセス群に含まれないか)が付与されており、§3のツリー表示スクリプトと同じロジックで`ProcessSelectionStrategy::Root`の妥当性をそのまま確認できる。`-DurationMinutes`で自動終了時間を指定することもできる(詳細はスクリプト冒頭のコメントヘッダを参照)。

---

## 4. 取得するタイミング(各アプリで)

1. アプリ未起動
2. アプリ起動直後、会議未参加(サインイン済み・トレイ常駐のみ)
3. テスト会議に参加した直後
4. 会議中に画面共有を開始した状態(該当する場合)
5. 会議を退出したがアプリは起動したまま
6. アプリを完全終了

**2→3、3→4の差分(新規に増えたPIDとその`ParentProcessId`)** が、「会議参加・画面共有によって何がどこの子として生成されるか」を示す。ここが今回の懸念(音声を出す実プロセスの特定)に直接効く部分。

---

## 5. 確認したいこと(チェックリスト)

* [ ] **Zoom**: サインイン後に常駐する`Zoom.exe`(トレイ)と、会議参加後の`Zoom.exe`(会議ウィンドウ)は同一プロセスか、親子関係にあるか、まったく別ツリーか
* [ ] **Zoom**: `ProcessSelectionStrategy::Root`が選ぶPIDは、会議音声が流れる過程で生成される子プロセスの祖先になっているか
* [ ] **Teams(新Teams)**: プロセスは単一か複数か。プロセス名は`ms-teams.exe`で一貫しているか(design.md/spike-plan.mdの「Teamsはマルチプロセス」前提が現行バージョンでも成立するか)
* [ ] **Chrome**: Google Meetのタブを開いた際、レンダラープロセスの`CommandLine`に含まれる`--type=renderer`等の引数から対象タブを絞り込めるか。タブごとに別プロセスになっているか
* [ ] いずれのアプリも、`ParentProcessId`が実際の起動元(`explorer.exe`、または前段のランチャープロセス)と一致しているか(昇格ヘルパーやジョブオブジェクト経由の起動で論理的な親子関係とズレていないか)

---

## 6. 結果の記録

CSVスナップショットとツリー出力を`spikes/spike-02-app-loopback/recon/`配下に保存し、簡単なMarkdownサマリ(観察された事実、`ProcessSelectionStrategy`への示唆)を添える。design.mdやspike-windows-01-02-detail-design.md §5.3を修正する必要が出た場合は、ここに記録してから反映する。

**この観察はSPIKE-02の`ActivateAudioInterfaceAsync`実装(§10の推奨実装順序ステップ2)より前に行っても、後に行っても支障はない** — 純粋にOSのプロセス一覧を見るだけの調査であり、Rustコードにもwindows crateにも依存しない。時間があるうちに先に済ませておくと、実装時の対象PID選択で手戻りが減る。
