# vsm-video

Windows標準のMedia FoundationでSpout2映像とVSM処理済み音声をH.264/AACのMP4へ保存するクレートです。OBS、FFmpeg、Spout SDKバイナリは製品実行時に使用しません。

## 呼び出し側

- `list_senders()` でSpout送信元を列挙します。`Preview::open(source)` は録画前の確認用で、`latest_frame()` が幅・高さ・取得QPCとtop-down BGRAを返します。
- `Recorder::start(path, VideoConfig { source, fps: 30, bitrate: 8_000_000, start_qpc_ns: 0 })` でエンコーダー準備後に開始します。`status().origin_qpc_ns` が実開始QPCです。
- `audio_sink()` は複製してVSMの音声出力callbackへ接続します。`sink.push(stereo_f32, timestamp - 300_000_000)` とし、48kHzステレオ処理音声と**入力時刻**を渡してください。モニター再生の予定時刻をそのまま渡すと映像に対して300ms遅れます。
- 停止ボタンのQPCを保存して `stop_at(qpc)` を呼びます。映像のcutoffを保ち、350msだけ遅れて到着する音声末尾を待ってからFinalizeします。
- `VideoReport.warning` がある場合も途中動画は正常に確定済みです。送信元終了・解像度変更・音声キュー溢れは動画だけ終了し、VSMの入力を止めません。書き込み・Finalize失敗は `Err` です。

AACは1024フレーム単位のため、デコーダーでは末尾に1パケット未満のpaddingが現れる場合があります。Spoutフレームが来ない区間と未取得の過去時刻は最後の受信フレームを保持します。4:2:0で必要な偶数の解像度へ右端・下端を最大1画素cropし、16～4096画素を受け付けます。

## 開発用チェック

Windows上で `cargo run -p vsm-video --release --example video-check --` の後ろに次を指定します。保存先ファイルは既存でないものを使用してください。

```
senders
record-synthetic synthetic.mp4 5
record-auto automatic-origin.mp4 5
record-spout-test shared-texture.mp4 5
record-spout-drop source-ended.mp4 5
inspect shared-texture.mp4
decode-check shared-texture.mp4
spout-sender 30
```

`record-spout-test` は開発用のD3D11共有テクスチャ送信者、Spout sender registry、named mutex、frame counterを経由して録画します。`record-spout-drop` は途中で送信者を閉じ、warning付きで途中動画を確定します。合成音声は256フレームpacketで100ms遅らせて送り、停止後に末尾を保持できるか確認します。`decode-check` はWindowsのデコーダーで全フレームを読み、動く画像・音声・末尾時刻を確認します。

Spout2プロトコルの参照は公式leadedge/Spout2のcommit `c2bcc12147711d12ace7d5f08e869d774d840f8a` のSpoutSenderNames/SpoutSharedMemory/SpoutFrameCount/SpoutDirectXです。
