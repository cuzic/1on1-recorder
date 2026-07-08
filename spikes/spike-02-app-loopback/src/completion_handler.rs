// spike-windows-01-02-detail-design.md §5.5
//
// エージル性についての注記(P0-3): ActivateAudioInterfaceAsyncの完了コールバックは、
// 我々の呼び出しスレッドとは別の、OS側のスレッドプール/RPCスレッドから呼ばれる。
// IAgileObject(メソッドを持たないマーカーインターフェース)を追加実装し、この
// COMオブジェクトがエージルであることを宣言する。
//
// 確認ポイント(§7): windows crateのバージョンによって#[implement]マクロの
// 属性記法・_Impl traitの命名が変わる可能性がある。実装前に該当バージョンの
// ドキュメント/サンプル(windows-rsリポジトリのaudioサンプル)を確認する。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use windows::core::IUnknown;
use windows::Win32::Media::Audio::{
    IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
    IActivateAudioInterfaceCompletionHandler_Impl,
};
// 訂正(cargo checkで実際に検出、§7の「要確認」を解消): 2点訂正する。
// (1) IAgileObjectはメソッドを持たないマーカーインターフェースだが、
//     windows-rs 0.58では`IAgileObject_Impl: Sized {}`という空のトレイトが
//     定義されており、`#[implement]`が要求する境界を満たすために明示的な
//     空implが必要(「_Implの実装が不要な場合が多い」という当初の想定は
//     このバージョンでは成立しない)。
// (2) `_Impl`トレイトは元の構造体(`CompletionHandler`)にではなく、
//     `#[implement]`マクロが生成する`CompletionHandler_Impl`という
//     ラッパー型に対して実装する。`CompletionHandler_Impl`は`Deref`で
//     `CompletionHandler`のフィールドへ透過的にアクセスできるため、
//     `self.tx`/`self.expired`はそのまま使える。孤立した最小再現で
//     `impl ... for CompletionHandler`(誤)と`impl ... for
//     CompletionHandler_Impl`(正)を比較し、後者のみ`cargo check`が通ることを
//     確認済み。
use windows::Win32::System::Com::IAgileObject_Impl;

#[windows::core::implement(IActivateAudioInterfaceCompletionHandler, windows::Win32::System::Com::IAgileObject)]
pub struct CompletionHandler {
    tx: Mutex<Option<std::sync::mpsc::Sender<windows::core::Result<IUnknown>>>>,
    /// §5.4のハードタイムアウト経路が有効な場合のみtrueになりうる。
    /// 既定(タイムアウトなし)の経路では常にfalseのまま。
    expired: Arc<AtomicBool>,
}

impl CompletionHandler {
    pub fn new(
        tx: std::sync::mpsc::Sender<windows::core::Result<IUnknown>>,
        expired: Arc<AtomicBool>,
    ) -> Self {
        Self {
            tx: Mutex::new(Some(tx)),
            expired,
        }
    }
}

impl IActivateAudioInterfaceCompletionHandler_Impl for CompletionHandler_Impl {
    fn ActivateCompleted(
        &self,
        activateoperation: Option<&IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let result: windows::core::Result<IUnknown> = (|| {
            let op = activateoperation
                .ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_POINTER))?;
            let mut hr = windows::core::HRESULT(0);
            let mut activated_interface: Option<IUnknown> = None;
            unsafe { op.GetActivateResult(&mut hr, &mut activated_interface)? };
            hr.ok()?;
            activated_interface
                .ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))
        })();

        if self.expired.load(Ordering::SeqCst) {
            // 呼び出し元は既にタイムアウトとして処理済み(P0-4)。結果の
            // インターフェースはdrop(=Release)するだけに留め、存在しないかも
            // しれない受信側へは送らない。
            tracing::warn!("late ActivateAudioInterfaceAsync completion after timeout; discarding");
            return Ok(());
        }

        if let Some(tx) = self.tx.lock().unwrap().take() {
            let _ = tx.send(result);
        }
        Ok(())
    }
}

impl IAgileObject_Impl for CompletionHandler_Impl {}
