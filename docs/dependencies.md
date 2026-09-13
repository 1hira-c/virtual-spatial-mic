# 依存物とライセンス

| 対象 | 依存物 | 記録 |
| --- | --- | --- |
| 音響処理 | Steam Audio 4.8.1 | [Apache-2.0](third-party/Steam-Audio-APACHE-2.0.txt)、[第三者notice](third-party/Steam-Audio-THIRDPARTY.md) |
| 単体版 | TauriほかのRustクレート | [notice](third-party/rust-studio-NOTICES.txt) |
| OBSプラグイン | OBS 32.2.2のAPI、Rustクレート | [OBSのGPL本文](third-party/OBS-GPL-2.0.txt)、[Rust notice](third-party/rust-obs-NOTICES.txt) |
| 口追跡モジュール | YL-ATG、Modular Avatar、VRChat Avatars SDK | 座標取得部に[YL-ATGのMITライセンス](../integrations/unity/Assets/VirtualSpatialMic/YL-ATG-LICENSE.txt)。SDKとModular Avatarは別途導入 |

Rustクレートの一覧は [rust-dependencies.json](third-party/rust-dependencies.json) に保存しています。SDKと依存クレートの実体はソースリポジトリに含めず、ビルド時に取得します。WebView2は単体版の実行環境です。

VSM独自部分は [MIT License](../LICENSE) です。OBSヘッダー由来の生成定義や第三者コンポーネントには元の条件が適用されます。[適用範囲とOBSプラグインの公開前確認事項](../LICENSES.md) を参照してください。

OBSのライセンス文書は、OBS APIと連携する配布物の確認に必要な資料として保持しています。単体アプリとOBSプラグインは別の配布物として扱います。
