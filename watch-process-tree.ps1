<#
.SYNOPSIS
  会議アプリ(Zoom/Teams/Chrome)のプロセスツリーの変化を定期的にJSONLへ記録する。

.DESCRIPTION
  spike02-process-tree-recon.md のワンショットPowerShellコマンドを自動化したもの。
  実行してそのまま放置し、Zoomへのサインイン・会議参加・画面共有・退出といった操作を
  行うだけで、対象プロセスの生成/消滅イベントがタイムスタンプ付きでJSONLへ記録される。
  「操作するタイミングに合わせて手動でPowerShellを叩く」必要がなくなる。

  記録されるイベント種別:
    initial_snapshot  起動時点で既に動いていたプロセス
    process_added     新規に生成されたプロセス(会議参加・画面共有等のタイミングで発生)
    process_removed   終了したプロセス
    heartbeat         生存確認用の定期記録(既定30秒ごと)

  root_candidate は、ProcessSelectionStrategy::Root(spike-windows-01-02-detail-design.md
  §5.3)と同じロジック――「親PIDが対象プロセス群に含まれないものを根とみなす」――を
  そのまま適用した値。会議参加時に生成された新規プロセスの root_candidate が false なら、
  既存のZoom.exeツリーの子として生成されている(Process Loopbackの
  INCLUDE_TARGET_PROCESS_TREEで自動的に拾われる)ことを意味する。

.PARAMETER IntervalSeconds
  ポーリング間隔(秒)。既定 2 秒。

.PARAMETER Pattern
  対象プロセス名の正規表現。既定 'zoom|teams|ms-teams|chrome'。

.PARAMETER OutFile
  出力先JSONLファイル。既定はカレントディレクトリに日時付きファイル名で作成。

.PARAMETER HeartbeatEverySeconds
  heartbeatイベントを記録する間隔(秒)。既定 30 秒。ログを開いたまま「まだ生きている」
  ことを確認するためのもので、解析上は無視してよい。

.PARAMETER DurationMinutes
  指定した場合、その分数が経過すると自動的に終了する。省略時は Ctrl+C で止めるまで動き続ける。

.EXAMPLE
  .\watch-process-tree.ps1

.EXAMPLE
  .\watch-process-tree.ps1 -IntervalSeconds 1 -Pattern 'zoom' -OutFile C:/temp/zoom_watch.jsonl

.EXAMPLE
  .\watch-process-tree.ps1 -DurationMinutes 15
#>
param(
    [double]$IntervalSeconds = 2.0,
    [string]$Pattern = 'zoom|teams|ms-teams|chrome',
    [string]$OutFile = "$PWD/proctree_watch_$(Get-Date -Format yyyyMMdd_HHmmss).jsonl",
    [int]$HeartbeatEverySeconds = 30,
    [int]$DurationMinutes = 0
)

function Get-Snapshot {
    Get-CimInstance Win32_Process |
        Where-Object { $_.Name -match $Pattern } |
        Select-Object ProcessId, ParentProcessId, Name, CreationDate, CommandLine
}

function Write-JsonlEvent {
    param($Record)
    ($Record | ConvertTo-Json -Compress -Depth 4) | Add-Content -Path $OutFile -Encoding utf8
}

function Test-RootCandidate {
    param($Snapshot, $TargetProcId)
    $target = $Snapshot | Where-Object { [int]$_.ProcessId -eq [int]$TargetProcId }
    if (-not $target) { return $true }
    $parent = $Snapshot | Where-Object { [int]$_.ProcessId -eq [int]$target.ParentProcessId }
    return -not [bool]$parent
}

Write-Output "Watching processes matching /$Pattern/ every ${IntervalSeconds}s -> $OutFile"
if ($DurationMinutes -gt 0) {
    Write-Output "Will stop automatically after $DurationMinutes minute(s). Ctrl+C to stop earlier."
} else {
    Write-Output "Ctrl+C to stop."
}

$prev = @{}
$lastHeartbeat = Get-Date
$startTime = Get-Date

# 起動時点で既に動いているプロセスは initial_snapshot として記録する
# (観測開始前からのプロセスも、後の解析で「元からいた」ものと区別できるようにする)
$initial = Get-Snapshot
foreach ($p in $initial) {
    $procId = [int]$p.ProcessId
    $prev[$procId] = $p
    Write-JsonlEvent @{
        ts             = (Get-Date).ToString("o")
        type           = "initial_snapshot"
        pid            = $procId
        ppid           = [int]$p.ParentProcessId
        name           = $p.Name
        creation_date  = "$($p.CreationDate)"
        command_line   = $p.CommandLine
        root_candidate = (Test-RootCandidate -Snapshot $initial -TargetProcId $procId)
    }
}
Write-Output "Initial snapshot: $($initial.Count) matching process(es)."

try {
    while ($true) {
        if ($DurationMinutes -gt 0 -and ((Get-Date) - $startTime).TotalMinutes -ge $DurationMinutes) {
            Write-Output "Duration limit reached. Stopping."
            break
        }

        Start-Sleep -Seconds $IntervalSeconds

        try {
            $cur = Get-Snapshot
        } catch {
            Write-Warning "Get-CimInstance failed transiently: $_"
            continue
        }

        $curMap = @{}
        foreach ($p in $cur) { $curMap[[int]$p.ProcessId] = $p }

        # 新規プロセス
        foreach ($procId in $curMap.Keys) {
            if (-not $prev.ContainsKey($procId)) {
                $p = $curMap[$procId]
                $parent = $cur | Where-Object { [int]$_.ProcessId -eq [int]$p.ParentProcessId }
                Write-JsonlEvent @{
                    ts             = (Get-Date).ToString("o")
                    type           = "process_added"
                    pid            = $procId
                    ppid           = [int]$p.ParentProcessId
                    parent_name    = $(if ($parent) { $parent.Name } else { $null })
                    name           = $p.Name
                    creation_date  = "$($p.CreationDate)"
                    command_line   = $p.CommandLine
                    root_candidate = (Test-RootCandidate -Snapshot $cur -TargetProcId $procId)
                }
                Write-Output "[+] $($p.Name) pid=$procId ppid=$($p.ParentProcessId)"
            }
        }

        # 消滅プロセス
        foreach ($procId in $prev.Keys) {
            if (-not $curMap.ContainsKey($procId)) {
                $p = $prev[$procId]
                Write-JsonlEvent @{
                    ts   = (Get-Date).ToString("o")
                    type = "process_removed"
                    pid  = $procId
                    name = $p.Name
                }
                Write-Output "[-] $($p.Name) pid=$procId"
            }
        }

        $prev = $curMap

        if (((Get-Date) - $lastHeartbeat).TotalSeconds -ge $HeartbeatEverySeconds) {
            Write-JsonlEvent @{
                ts                = (Get-Date).ToString("o")
                type              = "heartbeat"
                tracked_pid_count = $curMap.Count
            }
            $lastHeartbeat = Get-Date
        }
    }
} finally {
    Write-Output "Stopped. Log written to $OutFile"
}
