# 依存物とライセンス

| 対象 | 依存物 | 記録 |
| --- | --- | --- |
| 音響処理 | Steam Audio 4.8.1 | [Apache-2.0](third-party/Steam-Audio-APACHE-2.0.txt)、[第三者notice](third-party/Steam-Audio-THIRDPARTY.md) |
| 単体版 | TauriほかのRustクレート | [notice](third-party/rust-studio-NOTICES.txt) |
| OBSプラグイン | OBS 32.2.2のAPI、Rustクレート | [GPLv3本文](third-party/GPL-3.0.txt)、[OBS由来の表記](third-party/OBS-NOTICE.txt)、[Rust notice](third-party/rust-obs-NOTICES.txt) |
| 口追跡モジュール | YL-ATG、Modular Avatar、VRChat Avatars SDK | 座標取得部に[YL-ATGのMITライセンス](../integrations/unity/Assets/VirtualSpatialMic/YL-ATG-LICENSE.txt)。SDKとModular Avatarは別途導入 |

Rustクレートの一覧は [rust-dependencies.json](third-party/rust-dependencies.json) に保存しています。SDKと依存クレートの実体はソースリポジトリに含めず、ビルド時に取得します。WebView2は単体版の実行環境です。

VSM独自部分は [MIT License](../LICENSE) です。OBSヘッダー由来の生成定義や第三者コンポーネントには元の条件が適用されます。[ライセンスの適用範囲](../LICENSES.md) を参照してください。

単体版の自作部分はMIT、OBSプラグインの組み合わせた生成物はGPL-3.0-onlyで配布します。OBS由来の定義はGPL-2.0-or-laterを保持します。

OBS向けSteam Audioは、IPPなどの任意依存を無効にしたソースビルドです。[ビルド条件](third-party/Steam-Audio-OPEN-NOTICE.txt)と[必須依存のライセンス](third-party/Steam-Audio-OPEN-DEPENDENCIES.txt)を同梱します。単体版は公式Steam Audio DLLを使い、その第三者noticeも保持します。

[MPL-2.0本文](third-party/MPL-2.0.txt)に従うRustクレートを含め、依存ソースはバイナリーと同じ場所の対応ソースアーカイブから入手できます。アーカイブ名とハッシュは配布物の `SOURCE.txt` を参照してください。
