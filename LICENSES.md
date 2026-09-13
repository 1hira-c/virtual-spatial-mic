# ライセンスの適用範囲

| 対象 | 適用するライセンス |
| --- | --- |
| VSM独自のソース、単体版の独自部分、ドキュメント | [MIT](LICENSE) |
| 口追跡モジュールの独自部分 | MIT。YL-ATG部分には同梱のMIT表記を保持 |
| OBSプラグインの組み合わせた生成物 | **GPL-3.0-only**。独自ソース部分のMITによる利用許諾も維持 |
| OBSヘッダー由来のAPI定義 | 元の **GPL-2.0-or-later**。第三者の部分をMITへ変更しない |
| Steam Audio、Rustクレートなど | 各コンポーネントのライセンスと著作権表示を保持 |

## 単体版

単体版はOBSにリンクしません。VSM独自部分はMITのまま配布します。同梱DLLや依存クレートすべてがMITになる、という意味ではありません。

依存するRustクレートにはMPL-2.0のものも含まれます。対象クレートのソースは配布物と同じ場所の対応ソースアーカイブに収録します。MPLの条件は対象部分に適用され、VSM独自部分のMITを変更しません。

## OBSプラグイン

OBSはGPL-2.0-or-laterです。VSMが組み合わせるSteam Audio等のApache-2.0との互換性を考慮し、プラグイン生成物の配布にはGPLv3を選択します。独自部分をMITで別途利用できることは変わりません。生成された `crates/vsm-obs/src/bindings.rs` などOBS由来の部分は、その対象外です。

OBS向けには、IPP・MKL・Embree・Radeon Rays・TrueAudio Next・FFTSを無効にしてSteam Audioをソースからビルドします。FFTにはPFFFTを使います。Intel IPPを含む公式のビルド済みDLLはOBS配布物へ同梱しません。

配布時はGPLv3本文、MITの著作権・許諾表示、OBS由来部分の表記、第三者ライセンスを保持します。無保証であることを含め、GPLv3の条件に従って利用・変更・再配布できます。

## 対応ソースの提供

バイナリーと同じダウンロード場所に、`virtual-spatial-mic-<version>-sources.zip` を追加料金なしで提供します。VSMの該当ソースとビルドスクリプト、Cargo.lockに対応するRust依存ソース、使用したSteam Audioと必須依存のソース・ビルド変更を含みます。アーカイブ名・ハッシュ・元のコミットは配布manifestと `SOURCE.txt` に記録します。

`tools/build-product.ps1 -Release` が対応ソースを生成します。ソースを省略する通常の開発用ビルドと、再配布用のパッケージを区別します。配布時には `SOURCE.txt` の案内どおりソースアーカイブも配置してください。

参照：[OBS公式のプラグイン公開方針](https://obsproject.com/forum/threads/forum-resource-and-ip-policy.178569/)、[OBS 32.2.2のライセンス表記](https://github.com/obsproject/obs-studio/blob/32.2.2/libobs/obs-module.h)、[Apache-2.0とGPLの互換性](https://www.apache.org/licenses/GPL-compatibility.html)。
