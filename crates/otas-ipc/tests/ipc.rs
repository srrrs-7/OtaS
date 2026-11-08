//! IPC 層の結合テスト。Unix ソケットの組を作り、サーバとクライアントを同じスレッドで動かす。

use otas_ipc::{
    IpcError, Remote, Sessions, TaskClient, connect_approver, connect_task, serve_approver,
    serve_task, verify_peer_uid,
};
use otas_ports::{InferRequest, Role};
use otas_testkit::{World, all_grants};
use otas_types::{Grant, GrantSet, Label, LabelSet, TaskId};
use serde_json::json;
use std::future::Future;
use tokio::net::UnixStream;
use tokio::task::LocalSet;

/// `LocalSet` の中でテスト本体を動かす（capnp-rpc は1スレッドで動く前提）。
fn run_local<F: Future<Output = ()>>(f: F) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    LocalSet::new().block_on(&rt, f);
}

/// タスクの窓口をソケット越しに公開し、クライアントを返す。
fn task_over_socket(sessions: &Sessions, task: TaskId) -> TaskClient {
    let (server_end, client_end) = UnixStream::pair().unwrap();
    // 接続元の確認：ソケットの組なので、相手は自分と同じユーザー
    let uid = server_end.peer_cred().unwrap().uid();
    verify_peer_uid(&server_end, uid).unwrap();
    tokio::task::spawn_local(serve_task(server_end, sessions, task));
    connect_task(client_end)
}

#[test]
fn a_tool_call_goes_through_the_broker_and_labels_come_back() {
    run_local(async {
        let w = World::strong();
        let sessions = Sessions::new(w.broker.clone());
        let task = w.broker.spawn_task(all_grants());
        let client = task_over_socket(&sessions, task);

        let result = client
            .call_tool("mail.read", &json!({ "folder": "inbox" }))
            .await
            .unwrap();
        match result {
            Remote::Done(v) => assert_eq!(v["args"]["folder"], "inbox"),
            other => panic!("完了するはず: {other:?}"),
        }
        assert!(client.labels().await.unwrap().contains(Label::Private));
        assert_eq!(w.mail_read.calls(), 1);
    });
}

#[test]
fn denial_is_returned_as_a_result_with_a_reason() {
    run_local(async {
        let w = World::strong();
        let sessions = Sessions::new(w.broker.clone());
        let task = w.broker.spawn_task(GrantSet::none());
        let client = task_over_socket(&sessions, task);

        let result = client.call_tool("mail.read", &json!({})).await.unwrap();
        assert!(matches!(result, Remote::Denied(reason) if reason.contains("権限")));
        assert_eq!(w.mail_read.calls(), 0);
    });
}

#[test]
fn only_the_approver_connection_can_approve_a_held_send() {
    run_local(async {
        let w = World::strong();
        let sessions = Sessions::new(w.broker.clone());
        let task = w.broker.spawn_task(all_grants());
        let client = task_over_socket(&sessions, task);

        client.call_tool("mail.read", &json!({})).await.unwrap();
        client.call_tool("web.fetch", &json!({})).await.unwrap();
        let held = client
            .call_tool("mail.send", &json!({ "to": "a@example.com" }))
            .await
            .unwrap();
        let id = held.pending().expect("承認待ちになるはず");
        assert_eq!(w.mail_send.calls(), 0);

        // 承認の窓口は、別の接続（人間向けの画面）にだけ公開する
        let (server_end, client_end) = UnixStream::pair().unwrap();
        tokio::task::spawn_local(serve_approver(server_end, w.broker.clone()));
        let approver = connect_approver(client_end);

        let pending = approver.pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, id);
        assert_eq!(pending[0].task, task);

        let done = approver.approve(id).await.unwrap();
        assert!(matches!(done, Remote::Done(_)));
        assert_eq!(w.mail_send.calls(), 1);
    });
}

#[test]
fn inference_over_ipc_respects_where_the_model_runs() {
    run_local(async {
        let w = World::strong();
        let sessions = Sessions::new(w.broker.clone());
        let task = w.broker.spawn_task(all_grants());
        let client = task_over_socket(&sessions, task);
        client.call_tool("mail.read", &json!({})).await.unwrap();
        client.call_tool("web.fetch", &json!({})).await.unwrap();

        let request = InferRequest {
            messages: vec![otas_ports::Message::text(Role::User, "要約して")],
            max_tokens: 128,
            ..Default::default()
        };
        let local = client.infer("local-small", &request).await.unwrap();
        assert!(matches!(local, Remote::Done(text) if text.contains("local-small")));

        let cloud = client.infer("cloud-large", &request).await.unwrap();
        assert!(cloud.pending().is_some());
    });
}

#[test]
fn delegation_returns_a_narrower_capability() {
    run_local(async {
        let w = World::strong();
        let sessions = Sessions::new(w.broker.clone());
        let parent_task = w.broker.spawn_task(all_grants());
        let parent = task_over_socket(&sessions, parent_task);

        // 親の範囲内の権限なら、子の窓口が返る
        let child = parent
            .delegate(&GrantSet::new([Grant::Tool {
                pattern: "web.fetch".into(),
            }]))
            .await
            .unwrap();

        // 子は渡された権限しか使えない
        let denied = child.call_tool("mail.read", &json!({})).await.unwrap();
        assert!(matches!(denied, Remote::Denied(_)));
        let fetched = child.call_tool("web.fetch", &json!({})).await.unwrap();
        assert!(matches!(fetched, Remote::Done(_)));

        // 子の操作は子のタスクとして記録される（親を名乗る手段はない）
        let last = w.audit.records().last().unwrap().entry.clone();
        assert_ne!(last.task, parent_task);

        // 子が読んだもののラベルを、親が取り込む
        assert!(!parent.labels().await.unwrap().contains(Label::Untrusted));
        parent.absorb(&child).await.unwrap();
        assert!(parent.labels().await.unwrap().contains(Label::Untrusted));

        // 親より広い権限は渡せない
        let narrow = child.clone();
        let wider = narrow
            .delegate(&GrantSet::new([Grant::Tool {
                pattern: "*".into(),
            }]))
            .await;
        assert!(matches!(wider, Err(IpcError::Rpc(_))));
    });
}

#[test]
fn a_session_cannot_absorb_an_unrelated_task() {
    run_local(async {
        let w = World::strong();
        let sessions = Sessions::new(w.broker.clone());
        let a = task_over_socket(&sessions, w.broker.spawn_task(all_grants()));
        let b = task_over_socket(&sessions, w.broker.spawn_task(all_grants()));
        assert!(a.absorb(&b).await.is_err());
    });
}

#[test]
fn reading_data_over_ipc_carries_labels() {
    run_local(async {
        let w = World::strong();
        let sessions = Sessions::new(w.broker.clone());
        let client = task_over_socket(&sessions, w.broker.spawn_task(all_grants()));

        let ok = client
            .read_data("/data/mail/inbox/1.eml", &LabelSet::of([Label::Private]))
            .await
            .unwrap();
        assert_eq!(ok, Remote::Done(()));
        assert!(client.labels().await.unwrap().contains(Label::Private));

        let outside = client
            .read_data("/etc/shadow", &LabelSet::empty())
            .await
            .unwrap();
        assert!(matches!(outside, Remote::Denied(_)));
    });
}

#[test]
fn a_peer_with_another_uid_is_rejected() {
    run_local(async {
        let (server_end, _client_end) = UnixStream::pair().unwrap();
        let uid = server_end.peer_cred().unwrap().uid();
        let err = verify_peer_uid(&server_end, uid.wrapping_add(1)).unwrap_err();
        assert!(matches!(err, IpcError::PeerRejected { .. }));
    });
}
