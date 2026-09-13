# 口追跡モジュール

[導入手順](Assets/VirtualSpatialMic/README.md)に従ってアバターへ追加してください。

インポート用パッケージは [VSM_Mouth_Tracking_0.2.0-preview.unitypackage](dist/VSM_Mouth_Tracking_0.2.0-preview.unitypackage) です。同じ内容のアセットを `Assets/VirtualSpatialMic` に置いています。アセットを直接使う場合は `.meta` も一緒にコピーしてください。パッケージと直接コピーの両方を重複導入する必要はありません。

口元の位置だけを取得します。6個のFloatと版識別用のIntはすべてローカル用で、同期パラメーターの消費は0 bitです。口の表情や唇の変形を追跡するものではありません。

Modular AvatarとVRChat Avatars SDKは別途導入してください。アバター本体や検証用シーンは含みません。座標取得部はYL-ATG（MIT）を使用しています。
