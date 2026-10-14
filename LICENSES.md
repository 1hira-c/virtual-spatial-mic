# ライセンスの適用範囲

| 対象 | 適用するライセンス |
| --- | --- |
| VSM独自のソース、単体版の独自部分、ドキュメント | [MIT](LICENSE) |
| 口追跡モジュールの独自部分 | MIT。YL-ATG部分には同梱のMIT表記を保持 |
| Rustクレート（Tauri、sofar、tungsteniteなど） | 各クレートのライセンス（MIT、Apache-2.0、BSD、Zlib、Unicodeなどの許容型と、下記のMPL-2.0）。[notice](docs/third-party/rust-NOTICES.txt) |
| sofarに含まれるlibmysofa由来のコード | [BSD 3-Clause](docs/third-party/libmysofa-BSD-3-Clause.txt) |
| 埋め込みHRTFデータ（SADIE II D1） | [Apache-2.0](docs/third-party/Apache-2.0.txt)、[表記](docs/third-party/SADIE-II-NOTICE.txt) |
| Spout受信処理の参照元（Spout2公式SDK） | [BSD 2-Clause](docs/third-party/Spout-BSD-2-Clause.txt)、[表記と参照元](docs/third-party/Spout-NOTICE.txt)。SDKのC++ライブラリ・DLLは含めない |
| TauriのHTML/CSS処理で使うMPL-2.0クレート5個 | [MPL-2.0](docs/third-party/MPL-2.0.txt)。未改変のソースを配布物と一緒に提供 |

Spoutの共有メモリー・D3D11共有テクスチャの受信は、公式実装の通信仕様を参考にRustで実装しています。参照元のBSD-2-Clauseの著作権表示・条件・免責文を同梱します。MP4の生成はWindows Media FoundationのシステムAPIを使用し、外部エンコーダーは同梱しません。

VSMはOBS Studioにリンクせず、OBSのコードやヘッダー由来の定義も含みません。OBSとは、Windowsの音声デバイス（アプリケーション音声キャプチャや仮想オーディオケーブル）と、OBS内蔵のWebSocketサーバーのプロトコルでやり取りするだけです。

自作部分のMITは、同梱する第三者コンポーネントをMITにするものではありません。各コンポーネントの著作権表示とライセンスを保持してください。無保証であることを含め、それぞれの条件に従って利用・変更・再配布できます。

## MPL-2.0のソース提供

`vsm.exe` には、Tauri（wry）経由でMPL-2.0のクレートが含まれます：cssparser、cssparser-macros、dtoa-short、option-ext、selectors。一覧は [rust-dependencies.json](docs/third-party/rust-dependencies.json) の `mpl_crates` に記録しています。

VSMはこれらを改変せずに使います。再配布用パッケージでは、crates.ioの元の `.crate` ファイル（Cargo.lockのSHA-256で照合済み）を `virtual-spatial-mic-<version>-mpl-sources.zip` にまとめ、バイナリーと同じダウンロード場所に追加料金なしで置きます。アーカイブ名・ハッシュ・元のコミットは配布物の `SOURCE.txt` と `manifest.json` に記録します。MPL-2.0の条件は対象ファイルにだけ適用され、VSM独自部分のMITを変えません。

`tools/build-product.ps1 -Release` がこのアーカイブを生成します。ソースを省略する通常の開発用ビルドと、再配布用のパッケージを区別します。配布時には `SOURCE.txt` の案内どおりアーカイブも配置してください。
