# ビルドと開発

## Windows

必要なものはRust 1.96.0、MSVC x64のリンクツール、Windows SDK、WebView2 Runtime、Gitです。Rustの版は `rust-toolchain.toml`、クレートは `Cargo.lock` に固定しています。C/C++のライブラリ、CMake、外部SDKのダウンロードは不要です。

リポジトリ直下で実行します。

```powershell
cargo fmt --all -- --check
cargo test --locked --release --workspace
.\tools\build-product.ps1
```

`out/packages/` に単体版・第三者notice・ハッシュ付きmanifestが作られます。既存の出力フォルダーは上書きしません。

音声・ネットワーク機器を使う検証は通常のテストから分離しています。通常の `cargo test` はマイク録音を開始しません。obs-websocketとの連動は、テスト内の偽サーバー（`127.0.0.1`）で検証します。OSCの通信テストを明示実行する場合は、OSの通信許可ダイアログが出ることがあります。

```powershell
cargo test --locked --release -p vsm-live loopback_transport_and_query_discovery -- --ignored
```

## 構成

| パス | 役割 |
| --- | --- |
| `crates/vsm-core` | 座標処理、HRTFバイノーラル処理（sofar）、WAV、保存記録の再処理 |
| `crates/vsm-live` | WASAPI入力・出力、OSCQuery・OSC受信、リングバッファと記録、obs-websocket連動 |
| `crates/vsm-video` | WindowsのSpout受信、映像プレビュー、Media FoundationのMP4録画 |
| `apps/desktop` | Rust/Tauriの単体版と静的HTML・CSS・JavaScript |
| `integrations/unity` | 口追跡モジュールのアセットと導入資料 |

音声処理はRust側で動作します。GUIには操作と状態通知を渡し、ブラウザーからマイクを取得しません。画面のビルドにNode.jsは不要です。

既存の保存記録・OSCアドレス・記録イベントには互換性のため旧識別子（`native_obs_*`、`obs.events.jsonl`、`obs_timestamp_ns`、`VBS/Ref` など）が残っています。表示名の変更と同時にこれらを書き換えないでください。

## HRTFデータ

空間処理は `crates/vsm-core/src/dsp.rs` の `Binaural` です。SADIE IIのSOFAファイルをzlib圧縮したものを `include_bytes!` でexeに埋め込み、初回使用時に一度だけ展開・解析します（releaseで約1秒）。単体版は起動時に別スレッドで先読みします。

入手元・ハッシュ・作り直す手順は [crates/vsm-core/assets/hrtf/README.md](../crates/vsm-core/assets/hrtf/README.md) を参照してください。debugビルドでも解析と畳み込みが遅くならないよう、`Cargo.toml` でsofarなどの依存だけ最適化しています。

## Linuxでの共通処理テスト

Linux x86-64では共通処理のテストを実行できます。Linuxの実マイク・モニターは製品の対応対象外です。

```sh
cargo test --locked --release -p vsm-core -p vsm-live
```

## Linux製品対応の方針（未実装）

Linux版を今後の対応対象とし、現行のWindows版を維持して進めます。現在動くのは共通処理のテストで、Linuxの実マイク・モニター・動画録画は未対応です。TauriのGUIをビルドできることと、収録機能が使えることは別に確認します。

映像処理は `crates/vsm-video` に分離済みですが、内部では映像取得・フレーム変換・エンコード・MP4保存がWindows実装へまとまっています。移植ではその境界を分け、タイムスタンプ、開始・停止、遅れて届く音声の取り込み、原音・位置の記録、ライブラリの操作を共通に保ちます。

| 範囲 | Linuxで必要な作業 |
| --- | --- |
| 音声入力・モニター | PipeWire等の入出力を追加し、現在のWASAPI経路と同じ時刻基準・停止動作を確認する |
| VRChatカメラ映像 | Proton/Wine上のカメラ映像を受け取れる経路を先に実証する。通常の画面キャプチャが撮影レンズの映像になるとは仮定しない |
| 映像エンコード | VA-API等のハードウェアAPIを検討し、対象GPUで実際にエンコードできることを確認する |
| 音声エンコード・MP4 | AAC実装と依存ライセンスを選定する。MP4のmuxはRust実装も候補。映像エンコーダーだけでは現在のH.264/AAC録画を置き換えられない |
| GUI・配布 | LinuxのTauri依存、保存・再生、OSC発見、パッケージと第三者noticeを確認する |

GPL/LGPLの依存を追加することを既定方針にはしません。PipeWireの本体はMIT、libvaのCOPYINGは許容型ですが、採用するRust binding、プラグイン、ドライバー、音声コーデックと配布物全体も個別に確認します。GStreamerや既存のSpout2PWをそのまま製品へ組み込む方針ではありません。Spout2PWはLGPL-2.1で、2026-04-23にアーカイブされています。

Windowsでは `MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS` を有効にしています。これはハードウェアMFTの使用を許可する設定で、現在の検証はGPUエンコーダーの選択を証明していません。選択されたエンコーダーの記録と、CPUへのフォールバックの判別も検証項目にします。現在のBGRA→NV12変換はCPUで行っており、GPUエンコードが選ばれても映像処理全体がGPU上で完結するわけではありません。

確認順は、LinuxのGUIと実音声入出力 → 実カメラの映像入力 → 合成映像・音声でエンコードと保存 → VRChatと実マイクで同期・方向・停止復旧 → 長時間録画です。WSLでのビルド・共通テストは、Proton/Wine・GPU・音声機器を使った検証の代わりにしません。

調査元（2026-10-05確認）：[Media FoundationのハードウェアMFT設定](https://learn.microsoft.com/en-us/windows/win32/medfound/mf-readwrite-enable-hardware-transforms)、[Spout2](https://github.com/leadedge/Spout2)、[Spout2PW](https://github.com/hoshinolina/spout2pw)、[PipeWireのCOPYING](https://github.com/PipeWire/pipewire/blob/master/COPYING)、[libvaのCOPYING](https://github.com/intel/libva/blob/master/COPYING)、[RustのMP4 reader/writer候補](https://github.com/alfg/mp4-rust)。候補ライブラリは未採用です。

## 依存物の更新

Rust依存を変更したらnoticeも更新してください。CIは、コミット済みのnoticeが `Cargo.lock` と一致するか確認します。

```powershell
python tools/rust_notices.py docs/third-party
```

このスクリプトは、単体版に含まれるクレートのライセンスを確認し、GPL・LGPLなどの選択肢しかないクレートがあれば失敗します。Pythonはnotice生成と再配布用パッケージの作成だけに使い、ビルドには不要です。

## 再配布用パッケージ

公開用リポジトリの変更をコミットしてから実行します。

```powershell
.\tools\build-product.ps1 -Release
```

`-Release` はnoticeが最新か確認したうえで、バイナリーと一緒に `virtual-spatial-mic-<version>-mpl-sources.zip` を生成します。単体版に含まれるMPL-2.0のクレートを、crates.ioの元の `.crate` ファイルのまま（Cargo.lockのSHA-256で照合）収録します。バイナリーと同じダウンロード場所にこのアーカイブを追加料金なしで置き、`SOURCE.txt` を残してください。

ライセンスの扱いは[依存物とライセンス](dependencies.md)を参照してください。テストで生成した音声、実機の記録、端末固有の設定、アクセス情報はコミットしません。
