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
}
