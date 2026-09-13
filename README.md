# vLLM–LiteLLM Model Reconciler

複数のvLLMの `/v1/models` を定期取得し、LiteLLMのDB deploymentへ同期するRustサービスです。モデル名はvLLMのIDをそのまま使い、同じモデルを提供する複数サーバーは同じLiteLLMモデルグループへ登録します。管理対象以外のモデルは変更しません。

## 起動

Rust 1.85以上でビルドできます。

```sh
cp examples/reconciler.yaml config.yaml
export LITELLM_MASTER_KEY='your-master-key'
export VLLM_1_API_KEY='your-vllm-key'
export VLLM_2_API_KEY='your-vllm-key'
# config.yaml の各URLを実環境に合わせる
cargo run --locked -- --config config.yaml --once --dry-run
cargo run --locked --release -- --config config.yaml
```

`--once` は1周期だけ実行し、失敗時は終了コード1を返します。`--dry-run` または設定の `dry_run: true` では発見・一覧取得と差分ログのみを行います。設定は起動時に読み込みます。サーバー設定を変えたらreconcilerを再起動してください。vLLMのモデル切替には再起動不要です。

Docker Composeでは、`.env.example` を `.env` にコピーして値を設定し、`config.yaml` と `examples/litellm.yaml` を編集します。

```sh
cp .env.example .env
cp examples/reconciler.yaml config.yaml
# 上記ファイルを編集してから起動
docker compose up -d --build
docker compose logs -f reconciler
```

vLLM本体はComposeに含めていません。コンテナから到達できるURLを設定してください。ホスト上のvLLMへDocker Desktopから接続する場合は `host.docker.internal` を使えます。社内固定APIの例も実環境に合わせて変更、または不要なら削除してください。DBパスワードは接続URLへ埋め込むため英数字で生成してください。

## 設定と認証

全設定例は [`examples/reconciler.yaml`](examples/reconciler.yaml) にあります。`servers[].id` は永続的かつ一意な英数字・`-_.`です。`base_url` はvLLMでは `/v1` まで、LiteLLMではポートまで指定します。URL内のユーザー名・パスワード・クエリは拒否します。未知の設定項目や重複IDも起動時に拒否します。

`api_key_env` を省略するとvLLMの認証なしで発見し、LiteLLMへは `api_key: EMPTY` を登録します。指定した環境変数が未設定・空なら起動を中止します。無効化したサーバーのキーは不要です。

Master Keyは `master_key_env` が指定する環境変数、またはその名前に `_FILE` を付けた環境変数から読みます。例：`LITELLM_MASTER_KEY_FILE=/run/secrets/litellm_master_key`。vLLMのキーも同様にファイルから読み込めます。両方ある場合は値の環境変数を優先します。

LiteLLMへ登録するvLLMキーは実値ではなく `os.environ/VLLM_1_API_KEY` のような参照です。**同じキーをLiteLLM側の環境変数にも設定してください。** `_FILE` の読み取りはreconcilerの機能です。LiteLLM側では通常の環境変数を別途用意します。キーの参照名はメタデータに保存し、APIのマスク済みキーとの比較による無限更新を防ぎます。同じ参照名のキー値をローテーションした場合は、各プロセスの環境も更新してください。

Authorization、API応答本文、接続先URLはログに出しません。モデル名とsource IDはログに出るため、秘密情報を含めないでください。管理APIは信頼できるネットワーク内でのみ利用し、Composeのポートはlocalhostへバインドしています。

## 同期と削除のルール

- `model_info.managed_by == vllm-reconciler` かつ `source_id`・`source_model_id`・`external_key` が整合するdeploymentだけを操作します。`db_model: false` は除外します。不完全な所有メタデータは自動修復せず残します。
- 論理キーは `(source_id, source_model_id)`。新規登録にはこの組から生成するUUID v5を `model_info.id` として渡します。応答消失後に再登録しても同一DB IDとなり、重複作成を防ぎます。既存deploymentは実際のIDを保持します。
- 同じ論理キーの重複など曖昧な既存状態を見つけた場合は、変更を中止します。自動で一方を削除しません。
- 追加・更新を先に行い、`GET /model/info` でdesired stateを確認してから削除へ進みます。登録エラーや反映遅延で確認できなければ、その周期の削除はすべて延期します。
- 正常な空リスト、モデル切替、設定からの削除、`enabled: false` は、最初に不在を観測してから `deletion_grace_seconds` 経過後に削除します。
- 発見に失敗したサーバーは、連続失敗数が `failure_threshold` 以上、かつ最初の失敗から猶予時間が経過した場合だけ削除します。正常応答で失敗状態はリセットします。不正なJSONやスキーマも発見失敗として扱います。
- 猶予タイマーと失敗回数はメモリに保持し、再起動時はゼロから開始します。再起動によって削除が早まることはありません。deployment状態はLiteLLMから復元し、自前DBは持ちません。
- 各HTTP要求は設定のタイムアウトで制限します。5xx、接続失敗、タイムアウトを最大3試行し、指数バックオフとjitterを挟みます。4xx（429を含む）はその周期内で再試行しません。次周期には再評価します。
- 同期は単一ワーカーで実行し、各周期完了から `interval_seconds` 後に次を開始します。SIGTERM／Ctrl-Cでは実行中の周期を完了して停止します。複数reconcilerの同時稼働はサポートしません。レプリカ数は1にしてください。

大量のサーバーやdeploymentでは周期と終了処理が長くなります。発見と変更は順次実行されるため、上限の目安は「HTTP操作数 × (タイムアウト × 3 + バックオフ)」です。停止猶予を環境に応じて設定してください。

## LiteLLM互換性

Composeでは `ghcr.io/berriai/litellm:main-v1.81.12-stable.2` に固定しています。API契約は同タグの[公式ソース](https://github.com/BerriAI/litellm/blob/v1.81.12-stable.2/litellm/proxy/management_endpoints/model_management_endpoints.py)を確認しました。

| 操作 | API |
|---|---|
| 一覧・登録確認 | `GET /model/info` |
| 作成 | `POST /model/new`（`model_info.id` に安定ID） |
| 更新 | `PATCH /model/{id}/update` |
| 削除 | `POST /model/delete`（`{"id":"..."}`） |

仕様書の旧 `POST /model/update` は採用タグで `litellm_params` のみ保存し、名前や所有メタデータは保存しないため、公式のPATCHエンドポイントを使用します。PATCHはメタデータをマージし、既存の追加フィールドを保持します。認証を外す変更では空値やnullではなく `EMPTY` を設定し、旧キーがマージで残るのを防ぎます。

LiteLLMの `general_settings.store_model_in_db: true` とDB接続が必要です。固定社内APIはLiteLLMのYAMLに残してください。DBモデルとYAMLモデルは併用できます（[公式モデル管理ドキュメント](https://docs.litellm.ai/docs/proxy/model_management)）。別バージョンへ変更する際は管理APIとメタデータの往復を実環境で検証してください。

## 監視

デフォルトはポート9090です。

- `/healthz`：HTTPプロセス稼働確認。
- `/readyz`：直近の同期が成功した場合に200、それ以外は503。vLLM発見失敗も同期失敗とします。判定は直近周期の結果で、リクエストごとのLiteLLM接続確認はしません。
- `/metrics`：Prometheus形式。`reconciler_vllm_up`、`reconciler_discovered_models`、`reconciler_managed_deployments`、`reconciler_reconcile_errors_total`、`reconciler_last_success_timestamp`、`reconciler_changes_total`。

ログは1行1JSONです。dry-run時は `dry_run: true` を付け、変更メトリクスは増やしません。エラー数とタイムスタンプはプロセス再起動でリセットします。

OpenWebUIの接続先は `http://litellm:4000/v1` とOpenWebUI用LiteLLMキーに統一します。新規モデルの表示にはvirtual key／Teamのアクセス許可とOpenWebUIのモデル一覧キャッシュも関係します。負荷分散はLiteLLMの `simple-shuffle` が担当します（[公式ドキュメント](https://docs.litellm.ai/docs/proxy/load_balancing)）。

## テスト

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

HTTPモックによる統合テストで登録、再起動後の冪等性、モデル切替、削除猶予、障害回復、固定モデル保護、更新、dry-run、リトライ、readinessを検証します。GPUや本物のLiteLLM／PostgreSQLはこのテストには含まれません。

実環境での受け入れ確認では、2台で同モデルを起動して `/v1/models` の表示と両バックエンドへのリクエスト分散を確認し、片方停止・両方停止・モデル切替を試してください。固定モデルが残ること、OpenWebUI用キーで新規モデルへアクセスできることも確認対象です。
