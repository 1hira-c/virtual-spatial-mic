# ライセンスの適用範囲

Virtual Spatial Mic独自のソースコード、ドキュメント、口追跡モジュールの独自部分は [MIT License](LICENSE) です。第三者のコード・アセット・生成されたAPI定義には、元の権利者の条件が引き続き適用されます。

- Steam Audio、Rustの依存クレート、YL-ATGのライセンスと著作権表示を保持します。
- `crates/vsm-obs/src/bindings.rs` はOBSヘッダー由来の生成されたAPI定義です。本体のMIT表記によって、OBS由来の部分を再ライセンスするものではありません。
- OBSプラグインの配布には、OBSのGPLに適合する条件も確認する必要があります。MITを選んだことだけで「OBSプラグインを含めてMITのみ」とは扱いません。OBSプラグインの配布条件と対応ソースの提供方法は、公開前の確認項目です。

単体版とOBSプラグインは別の配布物です。それぞれに必要な第三者ライセンスを同梱します。口追跡モジュールに含むYL-ATG部分は、同梱の `YL-ATG-LICENSE.txt` に従います。

参照：[OBS公式のプラグイン公開方針](https://obsproject.com/forum/threads/forum-resource-and-ip-policy.178569/)、[OBSのライセンス](https://github.com/obsproject/obs-studio/blob/32.2.2/COPYING)。
