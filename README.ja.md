# Paseo Sessions

[English](README.md) · [简体中文](README.zh-CN.md) · [日本語](README.ja.md) · [한국어](README.ko.md)

Paseo Sessions は、コーディングエージェントの作業を実行・レビューするためのネイティブ環境です。**Linux、macOS、Windows、Android、iOS** 向けのクライアントは Flutter、デーモンと `paseo` CLI は Rust で実装されています。デスクトップはローカルデーモンを管理でき、モバイルは別のマシンのデーモンへの接続専用です。

このチェックアウトのバージョンは **0.13.1** です。配布成果物は[このリポジトリのリリース](https://github.com/emotionbug/paseo/releases)を使用してください。上流のダウンロードはこのリポジトリのビルドではありません。自分でビルド・配布する場合は[リリース要件](docs/release.md)を確認してください。

## ホストをまたいだ作業と機能範囲

サイドバーは **ホスト → ディレクトリ → セッション** の構成です。ホストが1台だけでも専用の行があり、空のディレクトリも保持されます。セッションは最近のアクティビティ順に並びます。ホストとディレクトリは独立して折りたため、ディレクトリの折りたたみ状態は保存されます。別ホストの同じパスは別々に扱われます。

検索はホスト、ディレクトリ名・パス、セッション名、プロバイダーを対象とし、保存済みの折りたたみ設定を変更せずに一致項目の親を展開します。ディレクトリの追加ボタンは、そのホストのチェックアウトでセッションを作成します。プロジェクト追加ではホスト側のフォルダーブラウザーを使い、リモートパスをローカル OS のファイルダイアログで選択しません。ワークスペースは引き続きオーケストレーション、worktree、スクリプト、Command Center の移動に使いますが、サイドバーのグループ化設定ではありません。既存のセッション ID、履歴、ファイル、設定、保存データは保持され、チャットタブを閉じてもセッションはアーカイブされません。

- エージェントチャット、権限、プロバイダー/モデル選択、プロファイル、セッションのインポートと履歴。
- Markdown 表示、ファイル閲覧、ソース編集。Git diff は概要を先に表示し、パッチ本文は必要時に読み込みます。
- ワークスペースのセットアップとサービススクリプトはデーモン所有の PTY で実行します。アプリは**読み取り専用ログ**を表示し、対話型ターミナルエミュレーターは提供しません。
- スケジュール、MCP ツールとサーバー、接続ホスト、リモートディレクトリ選択。
- ネイティブデスクトップ統合とローカルデーモンのライフサイクル管理。モバイルはリモート接続のみ。
- 一般的な効果音の再生は維持します。音声認識、音声会話、TTS は提供しません。

製品の Web クライアント、Electron、Chromium、埋め込みブラウザー、ブラウザー自動化、実行可能な React UI プラグインの互換性はありません。外部リンクはアプリの外で開きます。未使用の Web サイトは削除し、ドキュメントはリポジトリに保持します。**Paseo のランタイム、SDK、ビルドツールに Node や TypeScript は不要です。** エージェント CLI は外部プログラムで、それぞれ独自のランタイムを必要とする場合があります。

組み込みプロバイダーと使用量の統合は Rust で実行されます。バックエンドプラグインはネイティブ実行ファイルと Rust の `paseo-plugin` SDK を使います。既存の JavaScript/TypeScript プラグインは新しい契約への移植が必要です。[プラグイン](docs/plugins.md)を参照してください。

## ソースからビルド・起動

Rust/Cargo、Flutter SDK、対象プラットフォームのネイティブツールチェーンを用意します。少なくとも1つのエージェント CLI をインストールして認証を設定してください：[Claude Code](https://docs.anthropic.com/en/docs/claude-code)、[Codex](https://github.com/openai/codex)、[Copilot](https://github.com/features/copilot/cli/)、[OpenCode](https://github.com/anomalyco/opencode)、[Pi](https://pi.dev)。詳細は[開発ガイド](docs/development.md)にあります。

```sh
cargo fetch --locked
cargo xtask build-daemon
cargo xtask dev app
```

開発用デーモンとネイティブクライアントを別々に起動する場合：

```sh
cargo xtask dev daemon
# 別のターミナルで：
cargo xtask dev app
```

開発環境はチェックアウト内の `.dev/paseo-home` とポート `6768` を使い、通常の `~/.paseo` とポート `6767` から分離します。ビルドとリリースのコマンドは Rust の `paseo-xtask` クレートで実行します。

```sh
cargo xtask build-flutter linux
cargo xtask build-flutter macos
cargo xtask build-flutter windows
cargo xtask build-flutter android
cargo xtask build-flutter android --arch arm64
cargo xtask build-flutter ios
```

デスクトップは対象 OS 上でビルドし、iOS には macOS/Xcode が必要です。成果物とビルド記述は `artifacts/flutter/` に出力されます。Android/iOS にデーモンは含まれません。Android の名称は **Paseo Sessions**、パッケージ名は `sh.paseo.sessions` です。デバッグ署名 APK や未署名 iOS アプリアーカイブはストア公開用リリースではありません。[Android](docs/android.md)と[リリース](docs/release.md)を参照してください。

ヘッドレスホストでは生成された `target/debug/paseo`（Windows は `paseo.exe`）を使います。`CARGO_TARGET_DIR` 設定時は対応するディレクトリを使い、生成されたランタイム資産を実行ファイルの隣に保持してください。

```sh
./target/debug/paseo onboard
./target/debug/paseo daemon start
./target/debug/paseo daemon status
```

`cargo xtask build-daemon` はデバッグ版と資産を生成します。`cargo xtask build-daemon --release` は `target/release/` に最適化された**ローカルビルド**と資産を生成しますが、公開リリースとしての検証を保証しません。`paseo onboard` は明示的な初期設定です。引数なしの `paseo` は CLI チャットを開き、インストールや初期設定は行いません。

通常のデータディレクトリは `~/.paseo` で、`PASEO_HOME` または `--home` で変更できます。`config.json`、エージェント履歴、プロジェクト/ワークスペース情報、接続資格情報は既存の永続化仕様を維持します。リモート接続には `--host`、必要に応じて `PASEO_PASSWORD` を使います。SSH 接続は既存デーモンに接続するだけで、インストールはしません。[Docker ガイド](docs/docker.md)はネイティブデーモンのみのコンテナーを構築します。HTTP ルートでアプリは配信しません。

## CLI と SDK

ホスト設定のエージェントプロファイルで **Use for CLI** を選ぶと、既定のプロバイダー、モデル、モード、思考レベルを設定できます。明示的な CLI オプションが優先され、矛盾したプロバイダー/モデル引数は接続前にエラーになります。

```sh
paseo
paseo --resume <session-id>
paseo run --provider codex "review this checkout"
paseo ls
paseo attach <session-id>
paseo send <session-id> "also add tests"
paseo import --provider pi <native-session-id>
paseo run --host workstation.local:6767 --cwd /workspace --provider codex "review the diff"
```

外部連携用 Rust SDK `paseo-client` は `packages/client`、共通の通信仕様とリレー暗号化を持つ `paseo-protocol` は `packages/protocol` にあります。Rust デーモンと Dart クライアントの通信仕様は維持します。従来の TypeScript SDK をラップせず Rust SDK に置き換えます。

## ソース構成と検証範囲

| 領域                                                                             | パス                                                    |
| -------------------------------------------------------------------------------- | ------------------------------------------------------- |
| Rust デーモン、CLI、プロバイダー、ストレージ、supervisor、デスクトップホスト IPC | `packages/daemon`                                       |
| 5プラットフォーム向け Flutter クライアント                                       | `packages/flutter_app`                                  |
| デスクトップのパッケージングとランタイム資産                                     | `packages/flutter_desktop_host`                         |
| ネイティブオーディオ統合                                                         | `packages/flutter_audio`                                |
| Rust SDK と共通プロトコル                                                       | `packages/client`、`packages/protocol`                  |
| ネイティブバックエンドプラグイン SDK                                           | `packages/plugin`                                      |
| Rust リレーと構文ハイライト                                                    | `packages/relay`、`packages/highlight`                  |
| Rust ビルド・リリースツール                                                    | `xtask`                                                |

ランタイム境界を変更する前に[アーキテクチャ](docs/architecture.md)、[開発](docs/development.md)、[テスト](docs/testing.md)、[リリース](docs/release.md)を確認してください。Linux ではネイティブデーモン/クライアントとデスクトップホストのライフサイクルの実行実績があります。Android ARM64 の release モードビルドは成功しましたが、デバッグ署名であり、インストールや端末実行は行っていません。この移行では Windows、macOS、iOS の実機実行はありません。各 OS の CI 定義は実行証拠ではありません。デスクトップの追加検証を含め、最新の証拠と制限はテスト文書を参照してください。

## ライセンスと配布

Paseo のソースは Apache-2.0 で、依存関係には各自のライセンスが適用されます。パッケージングでは Cargo の依存関係と同梱資産の帰属表示を生成します。これは遵守の認証でも対応ソースの提供申し出でもありません。表示を保持し、依存関係ごとの義務を確認してください。[配布要件](docs/release.md#distribution-requirements)を参照してください。

## コミュニティと関連プロジェクト

[Discord](https://discord.gg/jz8T2uahpH) · [Reddit](https://www.reddit.com/r/PaseoAI/) · [GitHub Sponsors](https://github.com/sponsors/boudra) · [スポンサー](https://paseo.sh/sponsor)

- [getpaseo/paseo-relay](https://github.com/getpaseo/paseo-relay)：独立した Elixir 製分散リレープロジェクト。
- [paseo-vscode](https://marketplace.visualstudio.com/items?itemName=hinnes.paseo-vscode)：VS Code 拡張。今回のネイティブ移行での互換性検証を保証するものではありません。
