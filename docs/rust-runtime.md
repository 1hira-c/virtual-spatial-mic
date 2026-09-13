# 製品構成

単体版とOBSプラグインは共通のRust処理を使います。単体版の画面にはTauriを使い、音声入出力と空間処理はRust側で行います。

- `standalone/`：`vsm.exe`、再処理用の `vsm-native.exe`、Steam Audioの `phonon.dll`、ライセンス資料
- `obs/vrc-binaural-studio/`：OBSプラグイン、`phonon.dll`、ライセンス資料

DLLは同梱された配置を保ってください。単体版はWindows x64とWebView2 Runtimeを必要とします。OBSプラグインのビルド対象APIはOBS 32.2.2です。正式な対応バージョン範囲は検証中です。

OBSはライブ処理を継続し、録画開始・停止に合わせて保存します。単体版を同時に起動する必要はありません。
