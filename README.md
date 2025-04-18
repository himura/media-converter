# Rust HTTP メディアサムネイルサーバー

## 起動方法

```bash
cargo run -- --base-path /mnt/nas/media
```

## 機能概要

- NAS 上の画像・動画・PSD ファイルからサムネイル（WebP）を生成し、HTTP で返す軽量サーバー。
- キャッシュはフロントに任せる

## サポート対象フォーマット

- 静止画
    - JPEG, PNG, GIF, WebP
    - PSD：レイヤー統合表示（flatten）にて対応
- 動画
    - MP4, WebM: 最初のキーフレームを採用

## 機能一覧

### 共通仕様

- `Last-Modified` ヘッダ: ファイルの最終更新日時に応じて返却

### サムネイル生成

画像・動画のサムネイルを生成して WebP フォーマットで返す。

#### エンドポイント

```
GET /thumbnail/<filename>?size=<size>
```

#### パラメータ

- `size=small|medium|large`
    - デフォルト `medium`

### コンテンツ配信

画像を Web 閲覧用に最適化して配信する。

- 静止画: 解像度を維持して WebP に変換
- 動画: 最初のキーフレームを WebP に変換

#### エンドポイント

```
GET /media/<filename>
```

### ファイル配信

ファイルをそのまま配信する。手元環境用。

#### エンドポイント

```
GET /raw/<filename>
```

## 技術選定

| 項目 | 採用技術 / crate |
|------|-------------------|
| 言語 | Rust |
| HTTP サーバー | [actix-web](https://crates.io/crates/actix-web) |
| 画像処理 | [image](https://crates.io/crates/image) |
| PSD 読み込み | [psd](https://crates.io/crates/psd) |
| 動画処理 | [ffmpeg-next](https://crates.io/crates/ffmpeg-next) + FFmpeg CLI 依存なし |
| ログ・エラーハンドリング | thiserror, log |

---

## 今後の方針

### サムネイル生成の改善（動画）
- 最初のキーフレームが暗転（フェードイン）している場合、ユーザーが期待する内容でない
- 複数のキーフレームからスコアリングして「意味のある」サムネイルを選ぶ

### 改善案
1. 明るさスコアによるフィルタリング
2. フレームごとのヒストグラム/情報量スコアによる評価
3. クエリパラメータでサムネイル選択戦略を切り替え（`?strategy=best|first`）

### フォーマット対応
- [x] PSD flatten 対応
- [x] 動画対応（初期）
- [ ] PDF サムネイル（今後対応の可能性）

---

