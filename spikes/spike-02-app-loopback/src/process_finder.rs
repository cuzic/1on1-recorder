// spike-windows-01-02-detail-design.md §5.3
//
// Zoom/Teams/Chromeのようなマルチプロセスアプリでは、実行ファイル名だけでは
// 対象PIDが一意に決まらない。そのため、名前指定に加えて明示的なPID指定・
// 選択戦略を用意する。

use sysinfo::{Pid, System};

pub struct ProcessMatch {
    pub pid: u32,
    pub exe_name: String,
    pub parent_pid: u32,
    pub start_time: std::time::SystemTime,
}

/// プロセスツリーの選び方。Process Loopbackは対象プロセスとその子プロセスを
/// 含める仕組みのため、原則としてRoot(親を持たない、または最も祖先に近い
/// 候補)を選ぶ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ProcessSelectionStrategy {
    /// 同名プロセスの中で親PIDが同名プロセス群に含まれない(ツリーの根に近い)ものを選ぶ
    Root,
    /// start_timeが最も新しいものを選ぶ
    Newest,
    /// フォアグラウンドウィンドウを持つプロセスを優先する。ウィンドウが
    /// 見つからない場合はNewestにフォールバックする。
    Foreground,
}

/// 実行ファイル名(例: "Zoom.exe", "ms-teams.exe", "chrome.exe")で起動中
/// プロセスを検索する。複数候補が見つかった場合は全候補をログへ残し
/// (PID・親PID・開始時刻)、strategyに従って1件へ絞り込む。
pub fn find_process_by_name(
    exe_name: &str,
    strategy: ProcessSelectionStrategy,
) -> Option<ProcessMatch> {
    let mut sys = System::new_all();
    sys.refresh_all();

    let candidates: Vec<ProcessMatch> = sys
        .processes()
        .values()
        .filter(|p| p.name().eq_ignore_ascii_case(exe_name))
        .map(|p| ProcessMatch {
            pid: p.pid().as_u32(),
            exe_name: p.name().to_string_lossy().into_owned(),
            parent_pid: p.parent().map(|pp| pp.as_u32()).unwrap_or(0),
            start_time: std::time::UNIX_EPOCH + std::time::Duration::from_secs(p.start_time()),
        })
        .collect();

    for c in &candidates {
        tracing::info!(
            pid = c.pid,
            parent_pid = c.parent_pid,
            exe_name = %c.exe_name,
            "process_candidate_found"
        );
    }

    match strategy {
        ProcessSelectionStrategy::Root => {
            let pids: std::collections::HashSet<u32> = candidates.iter().map(|c| c.pid).collect();
            candidates
                .into_iter()
                .find(|c| !pids.contains(&c.parent_pid))
        }
        ProcessSelectionStrategy::Newest => {
            candidates.into_iter().max_by_key(|c| c.start_time)
        }
        ProcessSelectionStrategy::Foreground => {
            // TODO(§5.3): GetForegroundWindow + GetWindowThreadProcessIdで解決。
            // 見つからない場合はNewestへフォールバックする。
            candidates.into_iter().max_by_key(|c| c.start_time)
        }
    }
}

/// --target-pidが明示された場合はこちらを使い、名前解決を経由しない。
pub fn resolve_process_by_pid(pid: u32) -> Option<ProcessMatch> {
    let mut sys = System::new_all();
    sys.refresh_all();
    sys.process(Pid::from_u32(pid)).map(|p| ProcessMatch {
        pid,
        exe_name: p.name().to_string_lossy().into_owned(),
        parent_pid: p.parent().map(|pp| pp.as_u32()).unwrap_or(0),
        start_time: std::time::UNIX_EPOCH + std::time::Duration::from_secs(p.start_time()),
    })
}

/// 指定PIDが生存しているかをポーリングする(1秒間隔の定期ポーリングを既定実装とする)。
pub struct ProcessWatcher {
    target_exe_name: Option<String>, // --target-pid指定時はNone(名前では追跡しない)
    strategy: ProcessSelectionStrategy,
    current_pid: Option<u32>,
}

#[derive(Debug)]
pub enum ProcessWatchEvent {
    StillAlive(u32),
    Exited { old_pid: u32 },
    Restarted { old_pid: u32, new_pid: u32 },
    NotFound,
}

impl ProcessWatcher {
    pub fn new_by_name(exe_name: String, strategy: ProcessSelectionStrategy, initial_pid: u32) -> Self {
        Self {
            target_exe_name: Some(exe_name),
            strategy,
            current_pid: Some(initial_pid),
        }
    }

    pub fn new_by_pid(pid: u32) -> Self {
        Self {
            target_exe_name: None,
            strategy: ProcessSelectionStrategy::Root,
            current_pid: Some(pid),
        }
    }

    pub fn poll(&mut self) -> ProcessWatchEvent {
        let Some(current_pid) = self.current_pid else {
            return ProcessWatchEvent::NotFound;
        };

        if resolve_process_by_pid(current_pid).is_some() {
            return ProcessWatchEvent::StillAlive(current_pid);
        }

        // 現在のPIDは生存していない。
        match &self.target_exe_name {
            None => {
                // --target-pid指定時は名前で追跡しないため、単純にExitedとする。
                self.current_pid = None;
                ProcessWatchEvent::Exited { old_pid: current_pid }
            }
            Some(exe_name) => {
                if let Some(new_match) = find_process_by_name(exe_name, self.strategy) {
                    self.current_pid = Some(new_match.pid);
                    ProcessWatchEvent::Restarted {
                        old_pid: current_pid,
                        new_pid: new_match.pid,
                    }
                } else {
                    self.current_pid = None;
                    ProcessWatchEvent::Exited { old_pid: current_pid }
                }
            }
        }
    }
}
