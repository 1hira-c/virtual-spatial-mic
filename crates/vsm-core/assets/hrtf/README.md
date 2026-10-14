# 埋め込みHRTFデータ

`SADIE-II-D1-48K-24bit-256tap.sofa.zlib` は、SADIE II Database の被験者D1（Neumann KU100）の
`D1_48K_24bit_256tap_FIR_SOFA.sofa` をzlibで圧縮しただけのファイルです。データ自体は変更していません。
`crates/vsm-core/src/dsp.rs` が `include_bytes!` でexeに埋め込み、起動時に展開します。

| 項目 | 値 |
| --- | --- |
| 入手元 | https://sofacoustics.org/data/database/sadie/D1_48K_24bit_256tap_FIR_SOFA.sofa |
| プロジェクト | https://www.york.ac.uk/sadie-project/database.html （DOI 10.5281/zenodo.12092466） |
| 元ファイルのSHA-256 | `e6c72a84dd947b5ef75438ab96a9c2a32ed10f033472b9c4c11a49aff00a8a31` |
| 形式 | SOFA SimpleFreeFieldHRIR、48 kHz、256タップ、8802方向、半径1.2 m、Data.Delayはすべて0 |
| ライセンス | Apache-2.0（Copyright 2018, University of York）。表記は `docs/third-party/SADIE-II-NOTICE.txt` |

## 作り直す手順

```sh
curl -fLO https://sofacoustics.org/data/database/sadie/D1_48K_24bit_256tap_FIR_SOFA.sofa
python -c "import zlib,sys; open(sys.argv[2],'wb').write(zlib.compress(open(sys.argv[1],'rb').read(),9))" \
  D1_48K_24bit_256tap_FIR_SOFA.sofa SADIE-II-D1-48K-24bit-256tap.sofa.zlib
```

別のSOFAファイルに差し替える場合は、`dsp.rs` の `HRTF_SHA256` と `SPATIALIZER`、上の表、
`SADIE-II-NOTICE.txt` を更新してください。`dsp::tests::embedded_hrtf_integrity` が展開後のハッシュを検証します。
48 kHz以外のデータ、256タップ以外、Data.Delayが0でないデータは `dsp.rs` の読み込み時に拒否します。
