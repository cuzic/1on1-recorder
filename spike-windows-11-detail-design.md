# Windows スパイク(SPIKE-11: Audio Endpoint Registry)内部詳細設計書

* **文書ステータス**: Draft v0.1
* **作成日**: 2026-07-07
* **上位文書**: [design.md](design.md) §16.5、[spike-plan.md](spike-plan.md) SPIKE-11、[audio-device-state-architecture.md](audio-device-state-architecture.md)
* **姉妹文書**: [spike-windows-01-02-detail-design.md](spike-windows-01-02-detail-design.md)(SPIKE-01/02。COM実装の作法・落とし穴はこの文書での実機検証結果を引き継ぐ)
* **対象スパイク**: SPIKE-11(Audio Endpoint Registry — 全デバイスの観測状態追跡)
* **目的**: spike-plan.mdのSPIKE-11(仮説・検証手順・合否基準)と、audio-device-state-architecture.md §2〜5(`AudioEndpointSnapshot`/`AudioEvent`/`reduce`)の間にある**COM API実装層**を、そのままコーディング着手できる粒度まで詳細化する。

---

## 1. 前提と対象範囲

### 1.1 本書が埋める「隙間」

現状、3つの文書がそれぞれ別のレイヤーを担当しており、**実際にIMMNotificationClientをどう呼ぶか**だけがどこにも書かれていない。

| レイヤー | 文書 | 内容 |
|---|---|---|
| 仮説・合否基準 | spike-plan.md SPIKE-11 | 「`IMMNotificationClient`でendpoint単位の変化を追跡できる」という仮説と検証手順 |
| 状態・reducerの型 | audio-device-state-architecture.md §2〜5 | `AudioEndpointSnapshot`・`AudioEvent`・`reduce()`という**OS APIに依存しない**型設計 |
| **COM実装(本書)** | 本書 | `IMMNotificationClient`の`#[implement]`実装、登録/解除、初期スナップショット構築、コールバック→`AudioEvent`変換 |

audio-device-state-architecture.md §5.11は「実際のWASAPI実装に入る前に、まずFake WorkerでFSMを完成させるべき」としており、実COM実装は意図的に後回しにされていた。SPIKE-12(Fake Worker版FSM)が先に完了している前提で、本書は「Fake Workerを実WASAPI/COMへ差し替える」際に必要な、SPIKE-11固有のCOM実装を扱う。

### 1.2 対象外

* SPIKE-12のFSM/reducer本体(`CaptureBindingState`、`reduce()`) — audio-device-state-architecture.md §5で既存。本書はそこへ`AudioEvent`を供給する側だけを扱う
* マイク/Endpoint Loopback/Process Loopbackの録音キャプチャそのもの — spike-windows-01-02-detail-design.md
* プロダクションコードとしての品質担保(使い捨てコード)

### 1.3 SPIKE-01/02からの引き継ぎ事項

spike-windows-01-02-detail-design.mdの実装・`cargo check --target x86_64-pc-windows-gnu`による実機検証(Windows実機なしでも型チェックは可能)で、以下がすでに判明している。本書のCOM実装はこれらを最初から織り込む。

1. `windows`クレートには`"implement"` featureが必要(なければ`_Impl`トレイトが生成されない)
2. `windows-core`を`windows`とは別に直接の依存として追加する必要がある(`#[implement]`マクロが`windows_core::...`という直接のクレートパスを参照するため)
3. **`_Impl`トレイトは元の構造体ではなく、マクロが生成する`{構造体名}_Impl`ラッパー型に実装する**(`CompletionHandler`ではなく`CompletionHandler_Impl`が正、というのがSPIKE-02で実機検証済みの結論)
4. コールバックが我々の呼び出しスレッドとは別のCOMスレッドから飛んでくる場合、`IAgileObject`(空のマーカーインターフェース)を追加実装する必要があり、`IAgileObject_Impl`にも明示的な空implが要る
5. `IUnknown::cast()`を使うには`windows::core::Interface`をスコープに入れる

---

## 2. 全体構成

```text
spikes/
├─ spike-common/
│  └─ src/
│     └─ endpoint/              # 新規。SPIKE-01のdevice_select.rsからDeviceRoleを
│                                # 格上げし、SPIKE-11/12双方で共有する
│        ├─ mod.rs
│        ├─ ids.rs              # EndpointId, DataFlow, DeviceRole
│        └─ snapshot.rs         # DeviceState, AudioEndpointSnapshot
└─ spike-11-endpoint-registry/  # 新規バイナリクレート
   └─ src/
      ├─ main.rs                 # CLI, オーケストレーション
      ├─ notification_client.rs  # IMMNotificationClient実装
      ├─ initial_scan.rs         # 起動時の全endpoint列挙・初期スナップショット構築
      ├─ registry.rs             # AudioEndpointSnapshotレジストリの保持・更新
      └─ event_log.rs            # JSONL出力
```

**設計判断**: `EndpointId`/`DataFlow`/`DeviceRole`/`DeviceState`/`AudioEndpointSnapshot`はSPIKE-01の`device_select.rs`(`DeviceRole`)およびaudio-device-state-architecture.md §2.1で個別に導入された型と重複する。今後SPIKE-01/02側の`DeviceRole`もこの`spike-common::endpoint`へ差し替えることを前提に、ここで一本化する。`AudioEndpointSnapshot`の`default_roles: BTreeSet<DeviceRole>`が要求する`Ord`実装も、この一本化のタイミングで`DeviceRole`へ追加する(SPIKE-01の`DeviceRole`は現状`Ord`を持たない)。

---

## 3. 型定義(`spike-common::endpoint`)

### 3.1 識別子・列挙型

```rust
// spike-common/src/endpoint/ids.rs

/// IMMDevice::GetId()が返す文字列をそのまま保持する不透明な識別子。
/// 解析・分解して意味を読み取ろうとしないこと(Windowsの内部フォーマットに
/// 依存しないため)。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EndpointId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataFlow {
    Capture,
    Render,
}

/// SPIKE-01の`device_select::DeviceRole`と統合する。
/// `AudioEndpointSnapshot::default_roles: BTreeSet<DeviceRole>`のために
/// `Ord`を追加する(SPIKE-01時点では未実装だった)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, clap::ValueEnum)]
pub enum DeviceRole {
    Console,
    Multimedia,
    Communications,
}
```

### 3.2 デバイス状態・スナップショット

audio-device-state-architecture.md §2.1の型をそのまま採用する(名称・フィールドを変更しない)。

```rust
// spike-common/src/endpoint/snapshot.rs

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceState {
    Active,
    Disabled,
    NotPresent,
    Unplugged,
}

impl DeviceState {
    /// Win32の`DEVICE_STATE_XXX`定数からの変換。
    /// `DEVICE_STATE_ACTIVE`/`DISABLED`/`NOTPRESENT`/`UNPLUGGED`の4値のみが
    /// 定義されており、それ以外の値が来ることは想定しない(来た場合は
    /// `SpikeError`として扱いsummary.jsonへ記録する。§7参照)。
    pub fn from_win32(raw: u32) -> Result<Self, u32> {
        todo!("§7-1: windows::Win32::Media::Audio::DEVICE_STATE_* の実際の値を確認して実装する")
    }
}

#[derive(Debug, Clone)]
pub struct AudioEndpointSnapshot {
    pub id: EndpointId,
    pub flow: DataFlow,

    pub device_state: DeviceState,
    pub friendly_name: String,

    pub default_roles: std::collections::BTreeSet<DeviceRole>,
    pub volume_scalar: Option<f32>,
    pub muted: Option<bool>,

    /// Phase 1では未使用(将来のフォーマット変更検知用)。本スパイクでは
    /// 常に`None`のままでよい。
    pub format_fingerprint: Option<()>,

    pub revision: u64,
    pub last_observed_at_100ns: u64,
}
```

### 3.3 `EndpointOsEvent`(本書で新規定義)

spike-plan.mdのSPIKE-11検証手順は`EndpointOsEvent`という名前だけを挙げており、具体的な型は未定義だった。コールバックから消費スレッドへ渡す最小限の生イベントとして、次のように定義する。**この型はCOM実装の都合だけを反映した「生の通知」であり、`AudioEndpointSnapshot`の再構築や`AudioEvent`への変換は含まない**(それは§6の消費スレッドの責務)。

```rust
// spike-11-endpoint-registry/src/notification_client.rs

#[derive(Debug, Clone)]
pub enum EndpointOsEvent {
    DeviceAdded {
        endpoint_id: EndpointId,
        wake_qpc_100ns: u64,
    },
    DeviceRemoved {
        endpoint_id: EndpointId,
        wake_qpc_100ns: u64,
    },
    DeviceStateChanged {
        endpoint_id: EndpointId,
        new_state_raw: u32, // DeviceState::from_win32は消費スレッド側で行う(§3.2)
        wake_qpc_100ns: u64,
    },
    PropertyValueChanged {
        endpoint_id: EndpointId,
        /// PROPERTYKEYそのものは`windows`crateの型を素通しせず、
        /// (fmtid, pid)のタプルへ変換して保持する(スレッド間で送るため
        /// COM型を直接持ち回らない。§7-4参照)。
        property_key: (windows::core::GUID, u32),
        wake_qpc_100ns: u64,
    },
    DefaultDeviceChanged {
        flow: DataFlow,
        role: DeviceRole,
        endpoint_id: Option<EndpointId>, // 既定デバイスが存在しない場合はNone
        wake_qpc_100ns: u64,
    },
}
```

---

## 4. `IMMNotificationClient`実装

### 4.1 COMオブジェクト

Microsoft Learnの`IMMNotificationClient`解説は、コールバックをブロックしないこと・重い処理(再列挙やCOMオブジェクトの解放)を直接行わないことを要求している。したがって、5つのコールバックメソッドはいずれも「引数を`EndpointOsEvent`へ詰めてチャネルへ送るだけ」に徹する。

```rust
// spike-11-endpoint-registry/src/notification_client.rs (続き)

use windows::Win32::Media::Audio::{
    IMMNotificationClient, IMMNotificationClient_Impl, EDataFlow, ERole,
};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::core::PCWSTR;

/// P0-3(SPIKE-01/02から引き継ぎ): コールバックはOS側のスレッドプール/
/// COMスレッドから呼ばれるため、IAgileObjectを追加実装してエージルで
/// あることを宣言する。
#[windows::core::implement(IMMNotificationClient, windows::Win32::System::Com::IAgileObject)]
pub struct EndpointNotificationClient {
    tx: crossbeam_channel::Sender<EndpointOsEvent>,
    seq: std::sync::atomic::AtomicU64,
}

impl EndpointNotificationClient {
    pub fn new(tx: crossbeam_channel::Sender<EndpointOsEvent>) -> Self {
        Self { tx, seq: std::sync::atomic::AtomicU64::new(0) }
    }
}

/// `pwstr`(PCWSTR)をこの呼び出しの間だけ有効なローカルコピーとして
/// `EndpointId`(所有権を持つString)へ変換する。ポインタをコールバックの
/// 外へ持ち出さないこと(Windows側が呼び出し後に解放しうる)。
fn endpoint_id_from_pcwstr(pwstr: &PCWSTR) -> EndpointId {
    // TODO(§7-2): unsafe { pwstr.to_string() } (windows::core::PCWSTR::to_string)
    // で所有権のあるStringへ即座にコピーする。
    todo!("PCWSTR -> String へ即座にコピーする")
}

// 【実装時の注意】_ImplトレイトはEndpointNotificationClientではなく、
// マクロが生成するEndpointNotificationClient_Implへ実装する(§1.3の
// 引き継ぎ事項3)。EndpointNotificationClient_ImplはDerefで
// EndpointNotificationClientのフィールド(self.tx, self.seq)へ透過的に
// アクセスできる。
impl IMMNotificationClient_Impl for EndpointNotificationClient_Impl {
    fn OnDeviceStateChanged(&self, pwstrdeviceid: &PCWSTR, dwnewstate: u32) -> windows::core::Result<()> {
        let event = EndpointOsEvent::DeviceStateChanged {
            endpoint_id: endpoint_id_from_pcwstr(pwstrdeviceid),
            new_state_raw: dwnewstate,
            wake_qpc_100ns: now_100ns(),
        };
        let _ = self.tx.try_send(event); // 満杯でもブロックしない(§7-3)
        Ok(())
    }

    fn OnDeviceAdded(&self, pwstrdeviceid: &PCWSTR) -> windows::core::Result<()> {
        let event = EndpointOsEvent::DeviceAdded {
            endpoint_id: endpoint_id_from_pcwstr(pwstrdeviceid),
            wake_qpc_100ns: now_100ns(),
        };
        let _ = self.tx.try_send(event);
        Ok(())
    }

    fn OnDeviceRemoved(&self, pwstrdeviceid: &PCWSTR) -> windows::core::Result<()> {
        let event = EndpointOsEvent::DeviceRemoved {
            endpoint_id: endpoint_id_from_pcwstr(pwstrdeviceid),
            wake_qpc_100ns: now_100ns(),
        };
        let _ = self.tx.try_send(event);
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        pwstrdefaultdeviceid: &PCWSTR,
    ) -> windows::core::Result<()> {
        // pwstrdefaultdeviceidは既定デバイスが存在しない場合NULLになりうる
        // (§7-5)。PCWSTR::is_null()で判定してからOption化する。
        let endpoint_id = if pwstrdefaultdeviceid.is_null() {
            None
        } else {
            Some(endpoint_id_from_pcwstr(pwstrdefaultdeviceid))
        };
        let event = EndpointOsEvent::DefaultDeviceChanged {
            flow: map_data_flow(flow),
            role: map_device_role(role),
            endpoint_id,
            wake_qpc_100ns: now_100ns(),
        };
        let _ = self.tx.try_send(event);
        Ok(())
    }

    fn OnPropertyValueChanged(&self, pwstrdeviceid: &PCWSTR, key: &PROPERTYKEY) -> windows::core::Result<()> {
        let event = EndpointOsEvent::PropertyValueChanged {
            endpoint_id: endpoint_id_from_pcwstr(pwstrdeviceid),
            property_key: (key.fmtid, key.pid),
            wake_qpc_100ns: now_100ns(),
        };
        let _ = self.tx.try_send(event);
        Ok(())
    }
}

fn map_data_flow(flow: EDataFlow) -> DataFlow {
    todo!("§7-6: EDataFlowの実際のバリアント名(eCapture/eRender/eAll)を確認して分岐する")
}

fn map_device_role(role: ERole) -> DeviceRole {
    todo!("§7-6: ERoleの実際のバリアント名(eConsole/eMultimedia/eCommunications)を確認して分岐する")
}

fn now_100ns() -> u64 {
    // spike_common::timestamp::QpcClockを使う(spike-windows-01-02-detail-design.md §3.3)。
    todo!("QpcClock::query()をこのモジュール内でキャッシュして使う")
}
```

**チャネルのバックプレッシャー**: SPIKE-01/02のP1改善(bounded channel + `try_send`)をここでも踏襲する。デバイス変更イベントは音声フレームよりずっと低頻度なので、容量は小さくてよい(例: 64)。満杯時にdropが起きた場合は`endpoint_registry_drop_count`として記録する(§8)。

### 4.2 登録・解除

```rust
// spike-11-endpoint-registry/src/notification_client.rs (続き)

pub struct RegisteredNotificationClient {
    enumerator: windows::Win32::Media::Audio::IMMDeviceEnumerator,
    client: IMMNotificationClient,
}

impl RegisteredNotificationClient {
    pub fn register(
        enumerator: windows::Win32::Media::Audio::IMMDeviceEnumerator,
        tx: crossbeam_channel::Sender<EndpointOsEvent>,
    ) -> windows::core::Result<Self> {
        let handler = EndpointNotificationClient::new(tx);
        let client: IMMNotificationClient = handler.into();
        unsafe { enumerator.RegisterEndpointNotificationCallback(&client)? };
        Ok(Self { enumerator, client })
    }
}

impl Drop for RegisteredNotificationClient {
    fn drop(&mut self) {
        // 終了時に必ずUnregisterする。登録漏れ・解除漏れはリークとして
        // 合否基準(§8)で検出する。
        let _ = unsafe { self.enumerator.UnregisterEndpointNotificationCallback(&self.client) };
    }
}
```

`RegisteredNotificationClient`は、SPIKE-01の`ComApartment`/`StopSignal`と同じ「RAIIで確実に後始末する」パターンに揃える。`enumerator`/`client`はこの構造体を生成したスレッドのローカル値として保持し、他スレッドへは渡さない(P0-3方針の継続)。

---

## 5. 初期スナップショット構築(`initial_scan.rs`)

起動直後、コールバック登録より前に全endpointを列挙し、初期状態の`AudioEndpointSnapshot`レジストリを構築する。コールバック登録とこの初期列挙の間に発生した変更を取りこぼさないため、**先にコールバックを登録してから列挙する**(登録前に発生した変更はイベントとして飛んでこないため、列挙結果が正)か、**列挙してから登録し、登録の前後で再列挙して差分を吸収する**かのどちらかを選ぶ必要がある。本書は前者(先に登録)を採用する。

```rust
// spike-11-endpoint-registry/src/initial_scan.rs

use windows::Win32::Media::Audio::{
    IMMDeviceEnumerator, DEVICE_STATE_ACTIVE, DEVICE_STATE_DISABLED,
    DEVICE_STATE_NOTPRESENT, DEVICE_STATE_UNPLUGGED, EDataFlow, ERole,
};

pub fn scan_all_endpoints(
    enumerator: &IMMDeviceEnumerator,
) -> windows::core::Result<Vec<AudioEndpointSnapshot>> {
    // 1. enumerator.EnumAudioEndpoints(eAll, DEVICE_STATE_ACTIVE | DEVICE_STATE_DISABLED
    //        | DEVICE_STATE_NOTPRESENT | DEVICE_STATE_UNPLUGGED)
    //    -> IMMDeviceCollection (状態を問わずすべて列挙する。DEVICE_STATE_ACTIVEだけに
    //    絞ると「無効化されたデバイス」が観測できず、SPIKE-11の合否基準
    //    「Windows設定でのマイク/スピーカーの無効化・有効化」を検証できない)
    // 2. 各IMMDeviceについて:
    //    - GetId() -> EndpointId
    //    - GetState() -> DeviceState::from_win32
    //    - IPropertyStore経由でPKEY_Device_FriendlyName -> friendly_name
    //    - IAudioEndpointVolume(Activate経由)でGetMasterVolumeLevelScalar/GetMute
    //      -> volume_scalar/muted(§3.1相当。取得失敗時はNoneのまま許容する)
    // 3. flow(Capture/Render)は、EnumAudioEndpointsをeCapture/eRenderそれぞれ
    //    個別に呼ぶか、IMMEndpoint::GetDataFlow()で判定する
    todo!("§7-7: IPropertyStore/IAudioEndpointVolumeの具体的な呼び出し順序を実装する")
}

pub struct DefaultRouteMap {
    pub routes: std::collections::HashMap<(DataFlow, DeviceRole), Option<EndpointId>>,
}

pub fn scan_default_routes(
    enumerator: &IMMDeviceEnumerator,
) -> windows::core::Result<DefaultRouteMap> {
    // (Capture, Console)/(Capture, Multimedia)/(Capture, Communications)/
    // (Render, Console)/(Render, Multimedia)/(Render, Communications)の
    // 6通りそれぞれについてGetDefaultAudioEndpoint(flow, role)を呼ぶ。
    // 既定デバイスが存在しない場合はERR_NOTFOUND相当のエラーになるため、
    // それをOption::Noneへ変換する(§3.3のDefaultDeviceChangedと同じ扱い)。
    todo!("§7-8: GetDefaultAudioEndpointのエラー(存在しない場合)をNoneへ変換する")
}
```

---

## 6. 消費スレッド(`registry.rs`)

コールバックスレッドから届く`EndpointOsEvent`を受け取り、(a) 該当endpointの最新状態を**再取得**して`AudioEndpointSnapshot`レジストリを更新し、(b) JSONLへ記録し、(c) audio-device-state-architecture.md §5.2の`AudioEvent`(`EndpointObserved`/`EndpointRemoved`/`DefaultEndpointChanged`)へ変換してSPIKE-12のreducerへ渡す、という3つの責務を持つ。**重い処理(再列挙・プロパティ取得)はすべてこの消費スレッド側で行い、コールバック内では絶対に行わない**(§4.1の設計どおり)。

```rust
// spike-11-endpoint-registry/src/registry.rs

pub struct EndpointRegistry {
    snapshots: std::collections::HashMap<EndpointId, AudioEndpointSnapshot>,
    default_routes: DefaultRouteMap,
    revision_counter: u64,
}

impl EndpointRegistry {
    pub fn apply_os_event(
        &mut self,
        event: EndpointOsEvent,
        enumerator: &windows::Win32::Media::Audio::IMMDeviceEnumerator,
    ) -> Vec<AudioEvent> {
        // event種別ごとに:
        //  - DeviceAdded/DeviceStateChanged/PropertyValueChanged:
        //      該当endpoint_idを再列挙して最新のAudioEndpointSnapshotを構築し、
        //      revisionをインクリメントしてレジストリへ反映。
        //      AudioEvent::EndpointObserved { snapshot } を返す。
        //  - DeviceRemoved:
        //      レジストリから削除し、AudioEvent::EndpointRemoved { endpoint_id }を返す。
        //  - DefaultDeviceChanged:
        //      default_routesを更新し、
        //      AudioEvent::DefaultEndpointChanged { flow, role, endpoint_id }を返す。
        todo!("§6: イベント種別ごとの再取得・レジストリ更新・AudioEvent変換")
    }

    pub fn snapshot_all(&self) -> Vec<AudioEndpointSnapshot> {
        self.snapshots.values().cloned().collect()
    }
}
```

`AudioEvent`はaudio-device-state-architecture.md §5.2で定義済みの型をそのまま使う(本書では再定義しない)。SPIKE-12のFake Worker実装を、この`apply_os_event`が返す`AudioEvent`列で置き換えれば、そのままreducerへ接続できる。

---

## 7. 既知の不確実性・実装時の確認ポイント

spike-windows-01-02-detail-design.md §7と同じ位置づけで、詳細設計時点で判明した実装レベルの確認事項を残す。

1. **`DEVICE_STATE_*`定数の値**: `windows` crateでの実際の値・型(`u32`か newtype か)を`cargo doc`で確認してから`DeviceState::from_win32`を実装する。
2. **`PCWSTR`のライフタイム**: コールバック引数の`pwstrDeviceId`等は呼び出し中だけ有効な借用ポインタである。`windows::core::PCWSTR::to_string()`(unsafe)で即座に所有権のある`String`へコピーし、ポインタ自体をコールバックの外へ持ち出さない。
3. **チャネル満杯時のイベントdrop**: 音声フレームと異なり、デバイス変更イベントの取りこぼしは状態不整合に直結しうる(SPIKE-01のフレームdropとは重大性が異なる)。`try_send`失敗時にログへ警告を出すだけでなく、drop発生を検出したら**次回の定期的な全体再列挙(§5)で自己修復**する設計にするか、素直に`send`(ブロッキング)を使いキューを十分大きくするかは実機のイベント頻度を見て判断する。
4. **`PROPERTYKEY`の受け渡し**: `windows::Win32::Foundation::PROPERTYKEY`(`fmtid: GUID, pid: u32`)をスレッドをまたいで送るため、コールバック内でタプルへコピーする。`PKEY_Device_FriendlyName`等の既知キーとの比較は、消費スレッド側で行う。
5. **`OnDefaultDeviceChanged`のNULL PCWSTR判定**: 既定デバイスが存在しない場合、`pwstrDefaultDeviceId`がNULLになる。`PCWSTR::is_null()`で判定してから`Option`化すること(null前提のまま`to_string()`を呼ぶとクラッシュしうる)。
6. **`EDataFlow`/`ERole`の実際のバリアント**: `windows` crateでのenum表現(C的なu32定数か、Rustのenumか)を`cargo doc`で確認し、`map_data_flow`/`map_device_role`を実装する。
7. **`IPropertyStore`/`IAudioEndpointVolume`の呼び出し順序**: `IMMDevice::OpenPropertyStore(STGM_READ)`と`IMMDevice::Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)`の具体的な呼び出し順序・エラー処理(一部デバイスでは`IAudioEndpointVolume`が取得できない場合がある)を実装時に確認する。
8. **`GetDefaultAudioEndpoint`の「既定なし」エラー**: 既定デバイスが存在しない場合に返るHRESULT(`E_NOTFOUND`相当)を`Option::None`へ変換するハンドリングを実装する。

---

## 8. 出力ファイル・合否基準の自動化

spike-plan.mdのSPIKE-11検証手順(§3.2「USBマイク/ヘッドセットの抜き差し...」)に対応する。

```text
spikes/spike-11-endpoint-registry/out/{run_id}/
├─ endpoint_events.jsonl     # seq/event/endpoint_id/flow/old_state/new_state/observed_at_100ns
├─ registry_snapshot.json    # 終了時点の全AudioEndpointSnapshot
└─ summary.json
```

`endpoint_events.jsonl`の1行例:

```json
{"seq": 12, "event": "DeviceStateChanged", "endpoint_id": "{0.0.1.00000000}.{...}", "flow": "Capture", "old_state": "Active", "new_state": "Disabled", "observed_at_100ns": 1234567890}
```

`summary.json`の合否基準ブロック(spike-plan.mdの合否基準に対応):

```json
{
  "acceptance": {
    "all_changes_captured_by_id": true,
    "callback_duration_us": { "mean": 4.2, "p99": 12.0, "max": 30.0 },
    "callback_blocking_detected": false,
    "registry_matches_windows_state": null,
    "default_none_representable": true,
    "no_duplicate_registration": true,
    "no_leaked_registration": true
  }
}
```

`callback_duration_us`は、コールバックメソッドの入口と`try_send`直後でQPC時刻を取り、差分を計測して記録する(「callback内で重い処理をしていない」という合否基準を定量化する)。`registry_matches_windows_state`はWindowsのサウンド設定・コントロールパネルとの目視比較のため自動化せず`null`のままRESULT.mdへ手動記入する。

---

## 9. 実行手順(spike-plan.md手順との対応)

1. `cargo run -p spike-11-endpoint-registry --target x86_64-pc-windows-msvc`(またはgnu)で起動し、初期スナップショットが構築されることを確認する
2. USBマイク/ヘッドセットを抜き差しし、`endpoint_events.jsonl`に`DeviceAdded`/`DeviceRemoved`が記録されることを確認する
3. Windows設定でマイク/スピーカーを無効化・再有効化し、`DeviceStateChanged`(`Active`↔`Disabled`)を確認する
4. 既定マイク・既定スピーカー・既定通信デバイスをそれぞれ変更し、`DefaultDeviceChanged`を確認する(既定なしの状態も試す場合は全マイクを無効化して`endpoint_id: None`を確認する)
5. マイクmute・スピーカーmute/音量を変更し、`PropertyValueChanged`を確認する
6. Bluetoothヘッドセットの接続・切断を行い、上記イベントの組み合わせがどう飛んでくるかを観測する
7. 終了時に`RegisteredNotificationClient`のDropで`UnregisterEndpointNotificationCallback`が呼ばれることをログで確認する
8. `RESULT.md`へ判定を記入する(`registry_matches_windows_state`の目視比較結果を含む)

---

## 10. 見積り

spike-plan.mdのタイムボックス(3日)に対し、本書の粒度での想定内訳:

| 作業 | 目安 |
|---|---|
| `spike-common::endpoint`への型集約(§3)、SPIKE-01の`DeviceRole`置き換え | 0.5日 |
| `IMMNotificationClient`実装・登録/解除(§4) | 1日 |
| 初期スナップショット構築(§5) | 0.5日 |
| 消費スレッド・JSONL出力・CLI(§6, §8) | 0.5日 |
| 実機検証・RESULT.md(§9) | 0.5日 |
