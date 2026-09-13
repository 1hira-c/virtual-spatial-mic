# ビルドと開発

## Windows

必要なものはRust 1.96.0、MSVC x64のリンクツール、Windows SDK、WebView2 Runtimeです。Rustの版は `rust-toolchain.toml`、クレートは `Cargo.lock` に固定しています。依存SDKの取得にPython 3を使います。Pythonは開発時だけ必要です。

リポジトリ直下で実行します。

```powershell
python tools/bootstrap.py
cargo fmt --all -- --check
cargo test --locked --release --workspace
.\tools\build-product.ps1
```

`out/packages/` に単体版・OBSプラグイン・Steam Audio DLL・第三者notice・ハッシュ付きmanifestが作られます。既存の出力フォルダーは上書きしません。OBSを起動せずビルドできます。

音声・ネットワーク機器を使う検証は通常のテストから分離しています。通常の `cargo test` はマイク録音を開始しません。通信テストを明示実行する場合は、OSの通信許可ダイアログが出ることがあります。

```powershell
cargo test --locked --release -p vsm-live loopback_transport_and_query_discovery -- --ignored
```

## 構成

| パス | 役割 |
| --- | --- |
| `crates/vsm-core` | 座標処理、Steam Audioへの接続、WAV、保存記録の再処理 |
| `crates/vsm-live` | WASAPI入力・出力、OSCQuery・OSC受信、リングバッファと記録 |
| `crates/vsm-obs` | OBSのライブ録音・記録再生ソース |
| `apps/desktop` | Rust/Tauriの単体版と静的HTML・CSS・JavaScript |
| `integrations/unity` | 口追跡モジュールのアセットと導入資料 |

音声処理はRust側で動作します。GUIには操作と状態通知を渡し、ブラウザーからマイクを取得しません。画面のビルドにNode.jsは不要です。

OBSのABI定義は公式32.2.2ヘッダーから生成したものです。実行時はOBSが読み込んだ `obs.dll` を参照します。既存の保存記録・OSCアドレス・OBSソースIDには互換性のため旧識別子が残っています。表示名の変更と同時にこれらを書き換えないでください。

## Linuxでの共通処理テスト

Linux x86-64では共通処理のテストを実行できます。Linuxの実マイク・モニター・OBSプラグインは製品の対応対象外です。

```sh
python3 tools/bootstrap.py
cargo test --locked --release -p vsm-core -p vsm-live
```

## 依存物の更新

Steam Audioは `tools/dependencies.lock.json` のURLとSHA-256で固定します。Rust依存を変更したらnoticeも更新してください。

```powershell
python tools/rust_notices.py docs/third-party
```

ライセンスの扱いは[依存物とライセンス](dependencies.md)を参照してください。テストで生成した音声、実機の記録、端末固有の設定、アクセス情報はコミットしません。
