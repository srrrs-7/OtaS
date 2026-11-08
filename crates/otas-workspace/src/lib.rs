//! OtaS の作業領域と版管理（ハードリンクによる、特権を使わない実装）。
//!
//! # 置き場所
//!
//! ```text
//! <root>/live/              正本（論理パス /data に当たる）
//! <root>/versions/<n>/      版 n（正本をハードリンクで丸ごと複製したもの）
//! <root>/work/<task>/view/  タスクの作業領域（タスクから見える /data）
//! <root>/work/<task>/meta.json
//! <root>/meta/current       現在の版
//! <root>/meta/labels.json   パスごとのラベル
//! ```
//!
//! # 安全性の要点
//!
//! - 正本を書き換えるのはコミットと巻き戻しだけで、どちらも「一時ファイルに書いてから置き換える」。
//!   既存のファイルを上書きしないので、ハードリンクで共有している過去の版は壊れない。
//! - 作業領域のうち書き込める範囲は本物のコピー、読み取りだけの範囲はハードリンクにする。
//!   読み取りだけの範囲には、サンドボックス（Landlock）で書き込みを禁止する前提。
//! - コミットでは作業領域のファイルを中身としてコピーし、シンボリックリンクなど通常のファイル以外は
//!   拒否する（タスクが置いたリンクをデーモンがたどって、見せていないファイルを持ち出さないように）。
//! - コミットは、元にした版・作業領域・現在の正本の3つを比べる。衝突があれば何も反映しない。
//!
//! # 保証
//!
//! 版の保証は Weak として申告する。カーネルが強制するスナップショットではなく、
//! OtaS 以外のプロセスが正本のファイルをその場で書き換えると、ハードリンクを共有する版も変わるため。
//! Btrfs のスナップショットと OverlayFS による実装は、同じ trait の別実装として追加する。

use otas_ports::{
    Change, ChangeKind, CommitOutcome, ViewEntry, ViewSpec, Workspace, WorkspaceError,
    WorkspaceView,
};
use otas_types::{LabelSet, Level, TaskId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

const DATA_ROOT: &str = "/data";

/// 論理パス（`/data/...`）を、正本の根からの相対パスにする（`/data` 自体は空文字列）。
fn relative(logical: &str) -> Result<String, WorkspaceError> {
    let invalid =
        || WorkspaceError::InvalidPath(format!("{logical}（/data 以下を指定してください）"));
    let rest = if logical == DATA_ROOT {
        ""
    } else {
        logical.strip_prefix("/data/").ok_or_else(invalid)?
    };
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() {
        return Ok(String::new());
    }
    if rest
        .split('/')
        .any(|c| c.is_empty() || c == "." || c == "..")
    {
        return Err(invalid());
    }
    Ok(rest.to_string())
}

fn logical(rel: &str) -> String {
    if rel.is_empty() {
        DATA_ROOT.to_string()
    } else {
        format!("{DATA_ROOT}/{rel}")
    }
}

fn join(base: &Path, rel: &str) -> PathBuf {
    if rel.is_empty() {
        base.to_path_buf()
    } else {
        base.join(rel)
    }
}

/// `path` が `prefix` と同じか、その配下にあるか（相対パスどうし）。
fn within(prefix: &str, path: &str) -> bool {
    prefix.is_empty()
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn unsupported(path: &Path) -> WorkspaceError {
    WorkspaceError::Unsupported(format!(
        "{}（通常のファイルとディレクトリ以外は扱えません）",
        path.display()
    ))
}

/// ディレクトリを丸ごとコピーする（新しい実体を作る）。
fn copy_tree(src: &Path, dst: &Path) -> Result<(), WorkspaceError> {
    let meta = fs::symlink_metadata(src)?;
    if meta.is_dir() {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else if meta.is_file() {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dst)?;
    } else {
        return Err(unsupported(src));
    }
    Ok(())
}

/// ディレクトリを丸ごとハードリンクで複製する（実体は共有する）。
fn link_tree(src: &Path, dst: &Path) -> Result<(), WorkspaceError> {
    let meta = fs::symlink_metadata(src)?;
    if meta.is_dir() {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            link_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else if meta.is_file() {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::hard_link(src, dst)?;
    } else {
        return Err(unsupported(src));
    }
    Ok(())
}

fn remove_any(path: &Path) -> Result<(), WorkspaceError> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path)?,
        Ok(_) => fs::remove_file(path)?,
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// `base` の下の `rel` 以下にある通常のファイルを、相対パスで列挙する。
fn list_files(base: &Path, rel: &str) -> Result<BTreeMap<String, PathBuf>, WorkspaceError> {
    fn walk(
        path: &Path,
        rel: &str,
        out: &mut BTreeMap<String, PathBuf>,
    ) -> Result<(), WorkspaceError> {
        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        if meta.is_file() {
            out.insert(rel.to_string(), path.to_path_buf());
        } else if meta.is_dir() {
            for entry in fs::read_dir(path)? {
                let entry = entry?;
                let name = entry.file_name();
                let name = name
                    .to_str()
                    .ok_or_else(|| unsupported(&entry.path()))?
                    .to_string();
                let child = if rel.is_empty() {
                    name
                } else {
                    format!("{rel}/{name}")
                };
                walk(&entry.path(), &child, out)?;
            }
        } else {
            return Err(unsupported(path));
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    walk(&join(base, rel), rel, &mut out)?;
    Ok(out)
}

/// 通常のファイルならそのパス、存在しなければ `None`、それ以外はエラー。
fn regular(path: &Path) -> Result<Option<PathBuf>, WorkspaceError> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_file() => Ok(Some(path.to_path_buf())),
        Ok(_) => Err(unsupported(path)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// 2つのファイルの中身が同じか（どちらも存在しない場合も同じとみなす）。
fn same(a: Option<&Path>, b: Option<&Path>) -> Result<bool, WorkspaceError> {
    Ok(match (a, b) {
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
        (Some(a), Some(b)) => {
            fs::metadata(a)?.len() == fs::metadata(b)?.len() && fs::read(a)? == fs::read(b)?
        }
    })
}

/// 包含関係にある範囲を整理する（外側だけを残す）。
fn outermost(paths: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in paths {
        if paths.iter().any(|q| q != p && within(q, p)) || out.contains(p) {
            continue;
        }
        out.push(p.clone());
    }
    out
}

fn labels_for(map: &BTreeMap<String, LabelSet>, rel: &str) -> LabelSet {
    let mut labels = LabelSet::empty();
    for (path, l) in map {
        // 範囲の中にあるファイルのラベルと、範囲を含む上位のラベルの両方を取り込む
        if within(rel, path) || within(path, rel) {
            labels.join(l);
        }
    }
    labels
}

#[derive(Debug, Serialize, Deserialize)]
struct WorkMeta {
    base: u64,
    read: Vec<String>,
    write: Vec<String>,
}

/// ハードリンクによる作業領域と版管理。
#[derive(Debug)]
pub struct HardlinkStore {
    root: PathBuf,
    lock: Mutex<()>,
}

impl HardlinkStore {
    /// 置き場所を開く（なければ作る）。最初に開いたときは、正本の今の状態を版 1 として取り込む。
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, WorkspaceError> {
        let root = root.into();
        for dir in ["live", "versions", "work", "meta", "tmp"] {
            fs::create_dir_all(root.join(dir))?;
        }
        let store = Self {
            root,
            lock: Mutex::new(()),
        };
        if !store.current_path().exists() {
            link_tree(&store.live_dir(), &store.version_dir(1))?;
            store.write_meta_file(&store.current_path(), b"1")?;
        }
        Ok(store)
    }

    /// 正本の置き場所。
    pub fn live_dir(&self) -> PathBuf {
        self.root.join("live")
    }

    /// 範囲にラベルを付ける（既存のラベルに追加する）。デーモンの設定から使う。
    pub fn add_labels(&self, logical_path: &str, labels: &LabelSet) -> Result<(), WorkspaceError> {
        let rel = relative(logical_path)?;
        let _guard = self.guard();
        let mut map = self.load_labels()?;
        map.entry(rel).or_default().join(labels);
        self.save_labels(&map)
    }

    /// 範囲のデータに付いているラベル。
    pub fn labels_of(&self, logical_path: &str) -> Result<LabelSet, WorkspaceError> {
        let rel = relative(logical_path)?;
        let _guard = self.guard();
        Ok(labels_for(&self.load_labels()?, &rel))
    }

    fn guard(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn version_dir(&self, n: u64) -> PathBuf {
        self.root.join("versions").join(n.to_string())
    }

    fn work_dir(&self, task: TaskId) -> PathBuf {
        self.root.join("work").join(task.0.to_string())
    }

    fn current_path(&self) -> PathBuf {
        self.root.join("meta").join("current")
    }

    fn labels_path(&self) -> PathBuf {
        self.root.join("meta").join("labels.json")
    }

    fn temp_path(&self) -> PathBuf {
        self.root
            .join("tmp")
            .join(TaskId::new().0.simple().to_string())
    }

    /// 一時ファイルに書いてから置き換える。
    fn write_meta_file(&self, path: &Path, bytes: &[u8]) -> Result<(), WorkspaceError> {
        let tmp = self.temp_path();
        fs::write(&tmp, bytes)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    fn read_current(&self) -> Result<u64, WorkspaceError> {
        let text = fs::read_to_string(self.current_path())?;
        text.trim()
            .parse()
            .map_err(|_| WorkspaceError::Corrupt(format!("現在の版を読めません: {text:?}")))
    }

    fn load_labels(&self) -> Result<BTreeMap<String, LabelSet>, WorkspaceError> {
        match fs::read(self.labels_path()) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| WorkspaceError::Corrupt(format!("ラベルを読めません: {e}"))),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn save_labels(&self, map: &BTreeMap<String, LabelSet>) -> Result<(), WorkspaceError> {
        let bytes =
            serde_json::to_vec_pretty(map).map_err(|e| WorkspaceError::Corrupt(e.to_string()))?;
        self.write_meta_file(&self.labels_path(), &bytes)
    }

    fn load_meta(&self, task: TaskId) -> Result<WorkMeta, WorkspaceError> {
        let path = self.work_dir(task).join("meta.json");
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(WorkspaceError::NotFound(task));
            }
            Err(e) => return Err(e.into()),
        };
        serde_json::from_slice(&bytes)
            .map_err(|e| WorkspaceError::Corrupt(format!("作業領域の情報を読めません: {e}")))
    }

    /// 作業領域の変更（相対パス）。
    fn changes(
        &self,
        task: TaskId,
        meta: &WorkMeta,
    ) -> Result<Vec<(String, ChangeKind)>, WorkspaceError> {
        let base = self.version_dir(meta.base);
        let view = self.work_dir(task).join("view");
        let mut out = BTreeMap::new();
        for w in &meta.write {
            let before = list_files(&base, w)?;
            let after = list_files(&view, w)?;
            for (path, file) in &after {
                match before.get(path) {
                    None => {
                        out.insert(path.clone(), ChangeKind::Added);
                    }
                    Some(old) if !same(Some(old), Some(file))? => {
                        out.insert(path.clone(), ChangeKind::Modified);
                    }
                    Some(_) => {}
                }
            }
            for path in before.keys() {
                if !after.contains_key(path) {
                    out.insert(path.clone(), ChangeKind::Deleted);
                }
            }
        }
        Ok(out.into_iter().collect())
    }

    fn snapshot_live(&self) -> Result<u64, WorkspaceError> {
        let next = self.read_current()? + 1;
        link_tree(&self.live_dir(), &self.version_dir(next))?;
        self.write_meta_file(&self.current_path(), next.to_string().as_bytes())?;
        Ok(next)
    }
}

impl Workspace for HardlinkStore {
    fn snapshot_level(&self) -> Level {
        Level::Weak
    }

    fn current_version(&self) -> Result<u64, WorkspaceError> {
        let _guard = self.guard();
        self.read_current()
    }

    fn versions(&self) -> Result<Vec<u64>, WorkspaceError> {
        let _guard = self.guard();
        let mut versions: Vec<u64> = fs::read_dir(self.root.join("versions"))?
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
            .collect();
        versions.sort_unstable();
        Ok(versions)
    }

    fn begin(&self, task: TaskId, view: &ViewSpec) -> Result<WorkspaceView, WorkspaceError> {
        let write = outermost(
            &view
                .write
                .iter()
                .map(|p| relative(p))
                .collect::<Result<Vec<_>, _>>()?,
        );
        let read: Vec<String> = outermost(
            &view
                .read
                .iter()
                .map(|p| relative(p))
                .collect::<Result<Vec<_>, _>>()?,
        )
        .into_iter()
        .filter(|r| !write.iter().any(|w| within(w, r)))
        .collect();

        let _guard = self.guard();
        let dir = self.work_dir(task);
        if exists(&dir) {
            return Err(WorkspaceError::AlreadyExists(task));
        }
        let base = self.read_current()?;
        let base_dir = self.version_dir(base);
        let root = dir.join("view");
        fs::create_dir_all(&root)?;
        let labels = self.load_labels()?;

        let mut entries = Vec::new();
        for r in &read {
            let (src, dst) = (join(&base_dir, r), join(&root, r));
            if exists(&src) {
                link_tree(&src, &dst)?;
            } else {
                fs::create_dir_all(&dst)?;
            }
            entries.push(ViewEntry {
                logical: logical(r),
                physical: dst,
                writable: false,
                labels: labels_for(&labels, r),
            });
        }
        for w in &write {
            let (src, dst) = (join(&base_dir, w), join(&root, w));
            // 読み取り専用の範囲の中にある場合は、ハードリンクを外して本物のコピーに置き換える
            remove_any(&dst)?;
            if exists(&src) {
                copy_tree(&src, &dst)?;
            } else {
                fs::create_dir_all(&dst)?;
            }
            entries.push(ViewEntry {
                logical: logical(w),
                physical: dst,
                writable: true,
                labels: labels_for(&labels, w),
            });
        }
        // 作業領域の根が消えていたら作り直す（/data 全体を書き込み範囲にした場合）
        fs::create_dir_all(&root)?;

        let meta = WorkMeta { base, read, write };
        let bytes =
            serde_json::to_vec(&meta).map_err(|e| WorkspaceError::Corrupt(e.to_string()))?;
        fs::write(dir.join("meta.json"), bytes)?;

        Ok(WorkspaceView {
            root,
            base_version: base,
            entries,
        })
    }

    fn diff(&self, task: TaskId) -> Result<Vec<Change>, WorkspaceError> {
        let _guard = self.guard();
        let meta = self.load_meta(task)?;
        Ok(self
            .changes(task, &meta)?
            .into_iter()
            .map(|(path, kind)| Change {
                path: logical(&path),
                kind,
            })
            .collect())
    }

    fn commit(&self, task: TaskId, labels: &LabelSet) -> Result<CommitOutcome, WorkspaceError> {
        let _guard = self.guard();
        let meta = self.load_meta(task)?;
        let changes = self.changes(task, &meta)?;
        let dir = self.work_dir(task);
        if changes.is_empty() {
            fs::remove_dir_all(&dir)?;
            return Ok(CommitOutcome::NoChanges);
        }

        let live = self.live_dir();
        let base = self.version_dir(meta.base);
        let view = dir.join("view");

        // 3者比較：正本が元の版から変わっていないか、作業領域と同じ内容になっていれば反映してよい
        let mut conflicts = Vec::new();
        for (path, _) in &changes {
            let Ok(now) = regular(&live.join(path)) else {
                conflicts.push(logical(path));
                continue;
            };
            let before = regular(&base.join(path))?;
            let after = regular(&view.join(path))?;
            if !(same(now.as_deref(), before.as_deref())?
                || same(now.as_deref(), after.as_deref())?)
            {
                conflicts.push(logical(path));
            }
        }
        if !conflicts.is_empty() {
            return Ok(CommitOutcome::Conflict { paths: conflicts });
        }

        for (path, kind) in &changes {
            let target = live.join(path);
            match kind {
                ChangeKind::Added | ChangeKind::Modified => {
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    // 既存のファイルは上書きせず、新しい実体に置き換える（過去の版を壊さない）
                    let tmp = self.temp_path();
                    fs::copy(view.join(path), &tmp)?;
                    fs::rename(&tmp, &target)?;
                }
                ChangeKind::Deleted => match fs::remove_file(&target) {
                    Ok(()) => {}
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                },
            }
        }

        let mut map = self.load_labels()?;
        for (path, kind) in &changes {
            match kind {
                ChangeKind::Deleted => {
                    map.remove(path);
                }
                _ => map.entry(path.clone()).or_default().join(labels),
            }
        }
        self.save_labels(&map)?;

        let version = self.snapshot_live()?;
        fs::remove_dir_all(&dir)?;
        Ok(CommitOutcome::Applied {
            version,
            changes: changes
                .into_iter()
                .map(|(path, kind)| Change {
                    path: logical(&path),
                    kind,
                })
                .collect(),
        })
    }

    fn discard(&self, task: TaskId) -> Result<(), WorkspaceError> {
        let _guard = self.guard();
        let dir = self.work_dir(task);
        if !exists(&dir) {
            return Err(WorkspaceError::NotFound(task));
        }
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    fn rollback(&self, to: u64) -> Result<u64, WorkspaceError> {
        let _guard = self.guard();
        let src = self.version_dir(to);
        if !exists(&src) {
            return Err(WorkspaceError::NoSuchVersion(to));
        }
        let staged = self.temp_path();
        link_tree(&src, &staged)?;
        let old = self.temp_path();
        fs::rename(self.live_dir(), &old)?;
        fs::rename(&staged, self.live_dir())?;
        fs::remove_dir_all(&old)?;
        // ラベルは巻き戻さない（多めに付いている分には安全側）
        self.snapshot_live()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otas_types::Label;

    struct Fixture {
        root: PathBuf,
        store: HardlinkStore,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// 正本に notes.md と mail/1.eml がある状態から始める。
    fn fixture() -> Fixture {
        let root = std::env::temp_dir().join(format!("otas-ws-{}", TaskId::new()));
        let live = root.join("live");
        fs::create_dir_all(live.join("projects/a")).unwrap();
        fs::create_dir_all(live.join("mail")).unwrap();
        fs::write(live.join("projects/a/notes.md"), "v1").unwrap();
        fs::write(live.join("mail/1.eml"), "secret").unwrap();
        let store = HardlinkStore::open(&root).unwrap();
        Fixture { root, store }
    }

    fn view(read: &[&str], write: &[&str]) -> ViewSpec {
        ViewSpec {
            read: read.iter().map(|s| s.to_string()).collect(),
            write: write.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn live_text(f: &Fixture, rel: &str) -> Option<String> {
        fs::read_to_string(f.store.live_dir().join(rel)).ok()
    }

    #[test]
    fn opening_imports_the_current_data_as_version_one() {
        let f = fixture();
        assert_eq!(f.store.current_version().unwrap(), 1);
        assert_eq!(f.store.versions().unwrap(), vec![1]);
    }

    #[test]
    fn changes_stay_in_the_workspace_until_commit() {
        let f = fixture();
        let task = TaskId::new();
        let v = f
            .store
            .begin(task, &view(&["/data/mail"], &["/data/projects/a"]))
            .unwrap();
        assert_eq!(v.base_version, 1);
        fs::write(v.root.join("projects/a/notes.md"), "v2").unwrap();
        fs::write(v.root.join("projects/a/new.md"), "new").unwrap();

        let diff = f.store.diff(task).unwrap();
        assert_eq!(
            diff,
            vec![
                Change {
                    path: "/data/projects/a/new.md".into(),
                    kind: ChangeKind::Added
                },
                Change {
                    path: "/data/projects/a/notes.md".into(),
                    kind: ChangeKind::Modified
                },
            ]
        );
        // まだ正本は変わらない
        assert_eq!(live_text(&f, "projects/a/notes.md").unwrap(), "v1");

        let outcome = f.store.commit(task, &LabelSet::empty()).unwrap();
        assert!(matches!(outcome, CommitOutcome::Applied { version: 2, .. }));
        assert_eq!(live_text(&f, "projects/a/notes.md").unwrap(), "v2");
        assert_eq!(live_text(&f, "projects/a/new.md").unwrap(), "new");
        // 過去の版は、ハードリンクを共有していても壊れていない
        let v1 = fs::read_to_string(f.root.join("versions/1/projects/a/notes.md")).unwrap();
        assert_eq!(v1, "v1");
        // 作業領域は片付けられている
        assert!(matches!(
            f.store.diff(task),
            Err(WorkspaceError::NotFound(_))
        ));
    }

    #[test]
    fn concurrent_edits_to_the_same_file_conflict() {
        let f = fixture();
        let (a, b) = (TaskId::new(), TaskId::new());
        let va = f.store.begin(a, &view(&[], &["/data/projects/a"])).unwrap();
        let vb = f.store.begin(b, &view(&[], &["/data/projects/a"])).unwrap();
        fs::write(va.root.join("projects/a/notes.md"), "from a").unwrap();
        fs::write(vb.root.join("projects/a/notes.md"), "from b").unwrap();

        assert!(matches!(
            f.store.commit(a, &LabelSet::empty()).unwrap(),
            CommitOutcome::Applied { .. }
        ));
        let outcome = f.store.commit(b, &LabelSet::empty()).unwrap();
        assert_eq!(
            outcome,
            CommitOutcome::Conflict {
                paths: vec!["/data/projects/a/notes.md".into()]
            }
        );
        assert_eq!(live_text(&f, "projects/a/notes.md").unwrap(), "from a");
    }

    #[test]
    fn concurrent_edits_to_different_files_both_apply() {
        let f = fixture();
        let (a, b) = (TaskId::new(), TaskId::new());
        let va = f.store.begin(a, &view(&[], &["/data/projects/a"])).unwrap();
        let vb = f.store.begin(b, &view(&[], &["/data/projects/a"])).unwrap();
        fs::write(va.root.join("projects/a/notes.md"), "from a").unwrap();
        fs::write(vb.root.join("projects/a/other.md"), "from b").unwrap();

        assert!(matches!(
            f.store.commit(a, &LabelSet::empty()).unwrap(),
            CommitOutcome::Applied { .. }
        ));
        assert!(matches!(
            f.store.commit(b, &LabelSet::empty()).unwrap(),
            CommitOutcome::Applied { .. }
        ));
        assert_eq!(live_text(&f, "projects/a/notes.md").unwrap(), "from a");
        assert_eq!(live_text(&f, "projects/a/other.md").unwrap(), "from b");
    }

    #[test]
    fn deletion_is_committed() {
        let f = fixture();
        let task = TaskId::new();
        let v = f
            .store
            .begin(task, &view(&[], &["/data/projects/a"]))
            .unwrap();
        fs::remove_file(v.root.join("projects/a/notes.md")).unwrap();
        f.store.commit(task, &LabelSet::empty()).unwrap();
        assert!(live_text(&f, "projects/a/notes.md").is_none());
    }

    #[test]
    fn symlinks_are_never_followed_on_commit() {
        let f = fixture();
        let task = TaskId::new();
        let v = f
            .store
            .begin(task, &view(&[], &["/data/projects/a"]))
            .unwrap();
        std::os::unix::fs::symlink("/etc/passwd", v.root.join("projects/a/passwd")).unwrap();
        assert!(matches!(
            f.store.commit(task, &LabelSet::empty()),
            Err(WorkspaceError::Unsupported(_))
        ));
        assert!(live_text(&f, "projects/a/passwd").is_none());
    }

    #[test]
    fn discard_leaves_the_data_untouched() {
        let f = fixture();
        let task = TaskId::new();
        let v = f
            .store
            .begin(task, &view(&[], &["/data/projects/a"]))
            .unwrap();
        fs::write(v.root.join("projects/a/notes.md"), "v2").unwrap();
        f.store.discard(task).unwrap();
        assert_eq!(live_text(&f, "projects/a/notes.md").unwrap(), "v1");
        assert!(!v.root.exists());
    }

    #[test]
    fn rollback_restores_an_earlier_version() {
        let f = fixture();
        let task = TaskId::new();
        let v = f
            .store
            .begin(task, &view(&[], &["/data/projects/a"]))
            .unwrap();
        fs::write(v.root.join("projects/a/notes.md"), "v2").unwrap();
        f.store.commit(task, &LabelSet::empty()).unwrap();

        let version = f.store.rollback(1).unwrap();
        assert_eq!(version, 3);
        assert_eq!(live_text(&f, "projects/a/notes.md").unwrap(), "v1");
        assert_eq!(f.store.versions().unwrap(), vec![1, 2, 3]);
        assert!(matches!(
            f.store.rollback(99),
            Err(WorkspaceError::NoSuchVersion(99))
        ));
    }

    #[test]
    fn labels_flow_through_files() {
        let f = fixture();
        f.store
            .add_labels("/data/mail", &LabelSet::of([Label::Private]))
            .unwrap();

        // メールを見せた作業領域では、その範囲に私的ラベルが付いている
        let a = TaskId::new();
        let va = f
            .store
            .begin(a, &view(&["/data/mail"], &["/data/projects/a"]))
            .unwrap();
        let mail = va
            .entries
            .iter()
            .find(|e| e.logical == "/data/mail")
            .unwrap();
        assert!(mail.labels.contains(Label::Private));
        assert!(!mail.writable);

        // そのタスクが書いたファイルには、タスクのラベルが付く
        fs::write(va.root.join("projects/a/summary.md"), "要約").unwrap();
        let task_labels = LabelSet::of([Label::Private, Label::Untrusted]);
        f.store.commit(a, &task_labels).unwrap();

        // 次にそのディレクトリを見せたタスクには、同じラベルが付く
        let b = TaskId::new();
        let vb = f.store.begin(b, &view(&[], &["/data/projects/a"])).unwrap();
        assert_eq!(vb.entries[0].labels, task_labels);
        assert_eq!(f.store.labels_of("/data/projects").unwrap(), task_labels);
    }

    #[test]
    fn write_inside_read_is_a_real_copy() {
        let f = fixture();
        let task = TaskId::new();
        let v = f
            .store
            .begin(task, &view(&["/data"], &["/data/projects/a"]))
            .unwrap();
        // 書き込み範囲のファイルは、正本とは別の実体
        use std::os::unix::fs::MetadataExt;
        let in_view = fs::metadata(v.root.join("projects/a/notes.md")).unwrap();
        let in_live = fs::metadata(f.store.live_dir().join("projects/a/notes.md")).unwrap();
        assert_ne!(in_view.ino(), in_live.ino());
        // 読み取り範囲のファイルはハードリンク（実体を共有。書き込みはサンドボックスが禁止する）
        let mail_view = fs::metadata(v.root.join("mail/1.eml")).unwrap();
        let mail_live = fs::metadata(f.store.live_dir().join("mail/1.eml")).unwrap();
        assert_eq!(mail_view.ino(), mail_live.ino());
    }

    #[test]
    fn paths_outside_data_are_rejected() {
        let f = fixture();
        for bad in ["/etc", "/data/../etc", "/data/./x", "data/x", "/datax"] {
            let r = f.store.begin(TaskId::new(), &view(&[bad], &[]));
            assert!(matches!(r, Err(WorkspaceError::InvalidPath(_))), "{bad}");
        }
    }
}
