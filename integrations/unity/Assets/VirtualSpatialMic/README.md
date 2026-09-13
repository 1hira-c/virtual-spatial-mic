# Virtual Spatial Mic — 口追跡モジュール

VSM Mouth Tracking 0.2.0-preview。Modular Avatarでアバターに非破壊で追加する開発版です。

## 導入

VRChat Avatars SDKとModular Avatarを導入したアバターのプロジェクトを使います。先にプロジェクトをバックアップしてください。

1. このパッケージをインポートします。
2. `Assets/VirtualSpatialMic/Modules/VSM_Mouth_Tracking.prefab` をHierarchyのアバター直下に1個追加します。
3. モジュール内の `Mouth / Attachment / point` を選び、Sceneの移動ツールで口の中央へ合わせます。正面と横から位置を確認してください。初期位置はHeadボーンの中心です。これはアバターごとの初回調整です。
4. シーンを保存し、アバターをアップロードします。

調整するのはHierarchyに追加したモジュールのpointの位置です。親Attachmentや回転・スケールはそのままにします。口の表情変形を追跡するものではなく、頭に固定した発音位置を出力します。

更新時もモジュールを重複追加せず、既存の口元調整が残っていることを確認してください。取り外すときはアバター直下のモジュールを削除します。

元のFX・Expression Parameters・ボーンは直接編集せず、Modular Avatarがビルド時に統合します。SDK・Modular Avatar・アバター本体はこの配布物に含まれません。

座標取得部はYL-ATG © 2024 YozoraKurage（MIT）を使用しています。同梱の `YL-ATG-LICENSE.txt` を参照してください。
