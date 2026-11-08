//! 動作確認用のツール。設定の `[demo] tools = true` のときだけ登録する。本番では使わない。
//!
//! 実際には何もせず、呼ばれた名前と引数を返すだけ。ラベルと影響範囲だけは本物のツールと同じように
//! 宣言しているので、情報フロー制御（Rule of Two）の動きをエンドツーエンドで確かめられる。

use async_trait::async_trait;
use otas_ports::{ToolError, ToolOutput, ToolProvider, ToolSpec};
use otas_types::{Effect, Label, LabelSet};
use serde_json::json;
use std::sync::Arc;

struct DemoTool {
    spec: ToolSpec,
}

#[async_trait]
impl ToolProvider for DemoTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn call(&self, args: serde_json::Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: json!({ "tool": self.spec.name, "args": args }),
            labels: LabelSet::empty(),
        })
    }
}

fn tool(name: &str, description: &str, effect: Effect, labels: LabelSet) -> Arc<dyn ToolProvider> {
    Arc::new(DemoTool {
        spec: ToolSpec {
            name: name.into(),
            description: description.into(),
            effect,
            output_labels: labels,
        },
    })
}

/// 動作確認用のツール一式。
pub fn tools() -> Vec<Arc<dyn ToolProvider>> {
    vec![
        tool(
            "demo.read_private",
            "私的データを読んだことにする（内部で完結）",
            Effect::Internal,
            LabelSet::of([Label::Private]),
        ),
        tool(
            "demo.fetch_untrusted",
            "Web から取得したことにする（要求は外部に出て、結果は信頼できない）",
            Effect::External,
            LabelSet::of([Label::Untrusted]),
        ),
        tool(
            "demo.send",
            "外部に送信したことにする",
            Effect::External,
            LabelSet::empty(),
        ),
    ]
}
