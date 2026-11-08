@0xb4eacbc5c65cb368;

# OtaS の IPC スキーマ。
#
# 権限はケイパビリティ（偽造できない参照）として渡す。
# - サンドボックス内のタスクには、そのタスク専用の TaskSession だけを渡す。
#   TaskSession の操作は常にそのタスクとして扱われるので、他のタスクを名乗る手段がない。
# - 承認の操作（Approver）は、人間向けの画面にだけ渡す。タスクからは承認できない。
#
# ツールの引数・結果と権限は、otas-types の serde 表現（JSON 文字列）で運ぶ。

enum Label {
  untrusted @0;
  private @1;
  secret @2;
}

enum Role {
  system @0;
  user @1;
  assistant @2;
}

struct Message {
  role @0 :Role;
  content @1 :Text;
}

# 操作の結果。拒否はエラーではなく結果として返す（タスクが理由を読めるようにするため）。
struct CallResult {
  union {
    done @0 :Text;        # 結果（JSON。ツールは出力の内容、推論は本文の文字列、読み取りは null）
    pending @1 :Text;     # 承認待ちの ID
    denied @2 :Text;      # 拒否の理由
  }
}

struct PendingApproval {
  id @0 :Text;
  task @1 :Text;
  action @2 :Text;        # otas_types::Action の JSON
  reason @3 :Text;
}

# 1つのタスクに紐づく操作の窓口。
interface TaskSession {
  # ツールを呼ぶ。argsJson はツールの引数。
  callTool @0 (name :Text, argsJson :Text) -> (result :CallResult);

  # モデルで推論する。
  infer @1 (model :Text, messages :List(Message), maxTokens :UInt32) -> (result :CallResult);

  # データの読み取りを判定する。
  readData @2 (path :Text, labels :List(Label)) -> (result :CallResult);

  # タスクが積み上げたラベル。
  labels @3 () -> (labels :List(Label));

  # 子タスクを作り、その窓口を返す。権限は親の範囲内に限られる（grantsJson は GrantSet）。
  delegate @4 (grantsJson :Text) -> (child :TaskSession);

  # 子タスクの結果を受け取るときに、子のラベルを取り込む。
  absorb @5 (child :TaskSession) -> ();

  # モデルで推論する（ツールの呼び出しを含む）。requestJson は InferRequest、done は InferResponse の JSON。
  inferFull @6 (model :Text, requestJson :Text) -> (result :CallResult);

  # このタスクが呼べるツールの一覧（ToolSpec の配列の JSON）。
  tools @7 () -> (toolsJson :Text);

  # このタスクが使えるモデルの一覧（ModelCaps の配列の JSON）。
  models @8 () -> (modelsJson :Text);

  # 承認待ちになった操作が決着するまで待ち、結果を受け取る（1回だけ）。
  # done は元の操作と同じ形（ツールは出力の内容、推論は InferResponse の JSON、読み書きは null）。
  # 却下や承認後の拒否は denied で返す。
  waitApproval @9 (id :Text) -> (result :CallResult);
}

# 承認の窓口。人間向けの画面にだけ渡す。
interface Approver {
  pending @0 () -> (items :List(PendingApproval));
  approve @1 (id :Text) -> (result :CallResult);
  reject @2 (id :Text) -> ();
}

struct TaskInfo {
  id @0 :Text;
  statusJson @1 :Text;     # 状態（JSON。形はデーモンが決める）
}

# 人間向けの窓口。タスクの依頼と一覧、承認の窓口を提供する。
# デーモンが接続元のユーザーを確かめたうえで、人間向けの画面（CLI など）にだけ渡す。
interface Console {
  # タスクを依頼する（requestJson の形はデーモンが決める）。
  submit @0 (requestJson :Text) -> (taskId :Text);
  tasks @1 () -> (items :List(TaskInfo));
  approver @2 () -> (approver :Approver);

  # 作業領域の変更（JSON）。
  review @3 (taskId :Text) -> (changesJson :Text);
  # 作業領域の変更を正本にコミットする。結果（JSON）は反映した版か、衝突したパス。
  commit @4 (taskId :Text) -> (resultJson :Text);
  # 作業領域を捨てる。
  discard @5 (taskId :Text) -> ();
  # 正本の版の一覧（JSON）。
  versions @6 () -> (versionsJson :Text);
  # 正本を指定した版に戻す。結果（JSON）は新しい版。
  rollback @7 (version :UInt64) -> (resultJson :Text);

  # タスクの経過を listener に知らせ始める。
  # 最初にそれまでの出力と承認待ちを送り、その後は起きたときに送る。最後に finished を送って終わる。
  # 承認待ちは2回届くことがある（ID で重複を除く）。
  watch @8 (taskId :Text, listener :TaskListener) -> ();
}

# タスクの経過を受け取る側（人間向けの画面が実装する）。
interface TaskListener {
  # エージェントの標準出力（完全な行だけ。1回に複数行のことがある）。
  output @0 (lines :Text) -> ();
  # 承認待ちができた。
  approval @1 (item :PendingApproval) -> ();
  # タスクが終わった（状態の JSON）。これが最後の知らせ。
  finished @2 (statusJson :Text) -> ();
}
