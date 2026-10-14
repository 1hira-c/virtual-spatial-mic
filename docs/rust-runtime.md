# 製品構成

VSMは単体アプリ1つで動作します。画面にはTauriを使い、音声入出力と空間処理はRust側で行います。HRTFデータはexeに埋め込まれており、追加のDLLは不要です。

- `Virtual Spatial Mic/vsm.exe`：単体版
- `Virtual Spatial Mic/vsm-native.exe`：保存記録の一覧・確認・再処理を行うコマンドラインツール
- `Virtual Spatial Mic/licenses/`：第三者notice
- `Virtual Spatial Mic/OBS.md`：OBSで使う手順

Windows x64とWebView2 Runtimeが必要です。OBSへの音声は、Windowsの出力デバイスを経由して「アプリケーション音声キャプチャ」または仮想オーディオケーブルで渡します。録画開始・停止との連動にはOBS 28以降に内蔵されたWebSocketサーバーを使います。OBSプラグインのインストールは不要です。
