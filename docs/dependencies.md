# 依存物とライセンス

| 対象 | 依存物 | 記録 |
| --- | --- | --- |
| 空間処理（HRTFバイノーラル） | [sofar](https://github.com/andreiltd/sofar) 0.3.0（Rustのみ、MIT OR Apache-2.0） | libmysofa由来部分の[BSD 3-Clause](third-party/libmysofa-BSD-3-Clause.txt) |
| HRTFデータ | SADIE II Database 被験者D1（Neumann KU100、48 kHz） | [Apache-2.0](third-party/Apache-2.0.txt)、[表記と入手元](third-party/SADIE-II-NOTICE.txt) |
| Spout映像の受信 | Spout2公式実装を参考にしたRustの共有メモリー・D3D11受信処理 | 参照元の[BSD 2-Clause](third-party/Spout-BSD-2-Clause.txt)、[表記と参照元](third-party/Spout-NOTICE.txt) |
| 単体動画のMP4生成 | Windows Media Foundation（H.264・AAC） | WindowsのシステムAPIを使用。外部エンコーダーを同梱しない |
| OBSとの録画連動 | [tungstenite](https://github.com/snapview/tungstenite-rs)（obs-websocket v5のプロトコルで接続） | [Rust notice](third-party/rust-NOTICES.txt) |
| 単体版 | TauriほかのRustクレート | [Rust notice](third-party/rust-NOTICES.txt)、MPL-2.0分は[本文](third-party/MPL-2.0.txt) |
| 口追跡モジュール | YL-ATG、Modular Avatar、VRChat Avatars SDK | 座標取得部に[YL-ATGのMITライセンス](../integrations/unity/Assets/VirtualSpatialMic/YL-ATG-LICENSE.txt)。SDKとModular Avatarは別途導入 |

Rustクレートの一覧は [rust-dependencies.json](third-party/rust-dependencies.json) に保存しています。依存クレートの実体はソースリポジトリに含めず、ビルド時にcargoが取得します。HRTFデータは `crates/vsm-core/assets/hrtf/` に圧縮して保存し、exeに埋め込みます。WebView2は単体版の実行環境です。

製品のビルドに必要なのは、RustツールチェーンとMSVCのリンクツール（Windows SDKを含む）だけです。C/C++のライブラリ、CMake、Python、外部SDKのダウンロードは不要です。

VSM独自部分は [MIT License](../LICENSE) です。第三者コンポーネントには元の条件が適用されます。[ライセンスの適用範囲](../LICENSES.md) を参照してください。MPL-2.0のクレートは未改変で、そのソースは配布物と同じ場所のアーカイブから入手できます。アーカイブ名とハッシュは配布物の `SOURCE.txt` を参照してください。

Spoutの受信処理は、Spout2公式SDKの共有メモリー・共有テクスチャの通信仕様を参考にRustで実装しています。Spout SDKのC++ライブラリやDLLにはリンクせず、同梱もしません。参照元の著作権・BSD-2-Clauseの条件は [Spout-NOTICE.txt](third-party/Spout-NOTICE.txt) と同梱ライセンスに保持します。

単体動画録画は、WindowsのD3D11で取得した映像とVSM処理済みマイク音声を、Media FoundationでH.264・AACのMP4に保存します。FFmpegやOBSを呼び出す構成ではありません。

OBSの録画開始・停止との連動はobs-websocketのプロトコル（JSON over WebSocket）を使います。音声はWindowsの出力デバイス経由でOBSが取り込みます。OBSのコードやライブラリにはリンクしません。
