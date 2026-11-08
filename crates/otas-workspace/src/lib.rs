//! OtaS の作業領域と版管理。
//!
//! # 置き場所
//!
//! ```text
//! <root>/live/                 正本（論理パス /data に当たる）
//! <root>/versions/<n>/         版 n
//! <root>/work/<task>/          タスクの作業領域
//! <root>/meta/current          現在の版
//! <root>/meta/labels.json      パスごとのラベル
//! ```
//!
//! # 方式（[`StoreOptions`]）
//!
//! | 項目 | 選択肢 | 特権 |
//! |---|---|---|
//! | 作業領域（[`ViewMode`]） | `Copy`：書き込み範囲はコピー、読み取り範囲はハードリンク | 不要 |
//! |  | `Overlay`：書き込み範囲は OverlayFS（書いたときだけコピー）、読み取り範囲は版をそのまま見せる | 名前空間のサンドボックスが必要 |
//! | 版（[`VersionBackend`]） | `Hardlink`：正本をハードリンクで丸ごと複製（保証 Weak） | 不要 |
//! |  | `Btrfs`：読み取り専用のサブボリュームのスナップショット（保証 Strong） | Btrfs 上に置く。`Overlay` が必要 |
//!
//! # 安全性の要点
//!
//! - 正本を書き換えるのはコミットと巻き戻しだけで、どちらも「一時ファイルに書いてから置き換える」。
//!   既存のファイルを上書きしないので、ハードリンクで共有している過去の版は壊れない。
//! - コミットでは作業領域のファイルを中身としてコピーし、シンボリックリンクなど通常のファイル以外は拒否する
//!   （タスクが置いたリンクをデーモンがたどって、見せていないファイルを持ち出さないように）。
//! - コミットは、元にした版・作業領域・現在の正本の3つを比べる。衝突があれば何も反映しない。
//! - OverlayFS の上の層は、削除の印（whiteout）と、下の層を隠すディレクトリ（opaque）も変更として読む。

use otas_ports::{
    Change, ChangeKind, CommitOutcome, MountSource, ViewEntry, ViewSpec, Workspace, WorkspaceError,
    WorkspaceView,
};
use otas_types::{LabelSet, Level, TaskId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

const DATA_ROOT: &str = "/data";

/// 作業領域の方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewMode {
    /// 書き込み範囲はコピー、読み取り範囲はハードリンク（特権不要）。
    #[default]
    Copy,
    /// 書き込み範囲は OverlayFS、読み取り範囲は版をそのまま見せる（名前空間のサンドボックスが必要）。
    Overlay,
}

/// 版の方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionBackend {
    /// 正本をハードリンクで丸ごと複製する（保証 Weak）。
    #[default]
    Hardlink,
    /// Btrfs の読み取り専用スナップショット（保証 Strong）。`btrfs` コマンドを使う。
    Btrfs,
}

/// 置き場所の設定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StoreOptions {
    /// 作業領域の方式。
    pub view: ViewMode,
    /// 版の方式。
    pub versions: VersionBackend,
    /// 正本と作業領域のファイルの所有者（uid, gid）。名前空間のサンドボックスでタスクを
    /// このユーザーとして動かすときに指定する（デーモンは root で動く）。
    pub owner: Option<(u32, u32)>,
}

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

fn join_rel(prefix: &str, rel: &str) -> String {
    match (prefix.is_empty(), rel.is_empty()) {
        (true, _) => rel.to_string(),
        (_, true) => prefix.to_string(),
        _ => format!("{prefix}/{rel}"),
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
                let name = name.to_str().ok_or_else(|| unsupported(&entry.path()))?;
                walk(&entry.path(), &join_rel(rel, name), out)?;
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

/// OverlayFS の削除の印（whiteout）か：デバイス番号 0/0 のキャラクタデバイス。
fn is_whiteout(meta: &fs::Metadata) -> bool {
    meta.file_type().is_char_device() && meta.rdev() == 0
}

/// OverlayFS で、下の層の中身を隠すディレクトリ（opaque）か。
fn is_opaque(path: &Path) -> bool {
    let mut buf = [0u8; 8];
    rustix::fs::lgetxattr(path, "trusted.overlay.opaque", &mut buf).is_ok_and(|n| &buf[..n] == b"y")
}

fn btrfs(args: &[&std::ffi::OsStr]) -> Result<(), WorkspaceError> {
    let out = Command::new("btrfs")
        .args(args)
        .output()
        .map_err(|e| WorkspaceError::Unsupported(format!("btrfs コマンドを実行できません: {e}")))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(WorkspaceError::Unsupported(format!(
            "btrfs {} に失敗しました: {}",
            args.iter()
                .map(|a| a.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

/// 変更の1件（相対パス、種類、新しい中身の置き場所）。
type Pending = (String, ChangeKind, Option<PathBuf>);

#[derive(Debug, Serialize, Deserialize)]
struct WorkMeta {
    base: u64,
    read: Vec<String>,
    write: Vec<String>,
    #[serde(default)]
    view: ViewMode,
}

/// 作業領域と正本の版を、ファイルシステム上で管理する。
#[derive(Debug)]
pub struct FsStore {
    root: PathBuf,
    options: StoreOptions,
    lock: Mutex<()>,
}

impl FsStore {
    /// 特権を使わない既定の方式（コピーとハードリンク）で開く。
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, WorkspaceError> {
        Self::open_with(root, StoreOptions::default())
    }

    /// 方式を指定して開く（なければ作る）。最初に開いたときは、正本の今の状態を版 1 として取り込む。
    pub fn open_with(
        root: impl Into<PathBuf>,
        options: StoreOptions,
    ) -> Result<Self, WorkspaceError> {
        if options.versions == VersionBackend::Btrfs && options.view != ViewMode::Overlay {
            return Err(WorkspaceError::Unsupported(
                "Btrfs の版は OverlayFS の作業領域と組み合わせて使います（ハードリンクはサブボリュームをまたげないため）"
                    .into(),
            ));
        }
        let root = root.into();
        for dir in ["versions", "work", "meta", "tmp"] {
            fs::create_dir_all(root.join(dir))?;
        }
        let store = Self {
            root,
            options,
            lock: Mutex::new(()),
        };
        let live = store.live_dir();
        match options.versions {
            VersionBackend::Hardlink => fs::create_dir_all(&live)?,
            VersionBackend::Btrfs => {
                if exists(&live) {
                    // 既存の正本は Btrfs のサブボリュームでなければならない
                    btrfs(&["subvolume".as_ref(), "show".as_ref(), live.as_os_str()])?;
                } else {
                    btrfs(&["subvolume".as_ref(), "create".as_ref(), live.as_os_str()])?;
                }
            }
        }
        store.own_tree(&live)?;
        if !store.current_path().exists() {
            store.snapshot(1)?;
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

    fn unique_name() -> String {
        TaskId::new().0.simple().to_string()
    }

    /// 所有者を設定していれば、そのユーザーのものにする。
    fn own(&self, path: &Path) -> Result<(), WorkspaceError> {
        if let Some((uid, gid)) = self.options.owner {
            std::os::unix::fs::lchown(path, Some(uid), Some(gid))?;
        }
        Ok(())
    }

    fn own_tree(&self, path: &Path) -> Result<(), WorkspaceError> {
        if self.options.owner.is_none() {
            return Ok(());
        }
        self.own(path)?;
        if fs::symlink_metadata(path)?.is_dir() {
            for entry in fs::read_dir(path)? {
                self.own_tree(&entry?.path())?;
            }
        }
        Ok(())
    }

    fn create_dirs(&self, path: &Path) -> Result<(), WorkspaceError> {
        if exists(path) {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            self.create_dirs(parent)?;
        }
        fs::create_dir(path)?;
        self.own(path)
    }

    /// ディレクトリを丸ごとコピーする（新しい実体を作る）。
    fn copy_tree(&self, src: &Path, dst: &Path) -> Result<(), WorkspaceError> {
        let meta = fs::symlink_metadata(src)?;
        if meta.is_dir() {
            self.create_dirs(dst)?;
            for entry in fs::read_dir(src)? {
                let entry = entry?;
                self.copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
            }
        } else if meta.is_file() {
            if let Some(parent) = dst.parent() {
                self.create_dirs(parent)?;
            }
            fs::copy(src, dst)?;
            self.own(dst)?;
        } else {
            return Err(unsupported(src));
        }
        Ok(())
    }

    /// ディレクトリを丸ごとハードリンクで複製する（実体は共有する）。
    fn link_tree(&self, src: &Path, dst: &Path) -> Result<(), WorkspaceError> {
        let meta = fs::symlink_metadata(src)?;
        if meta.is_dir() {
            self.create_dirs(dst)?;
            for entry in fs::read_dir(src)? {
                let entry = entry?;
                self.link_tree(&entry.path(), &dst.join(entry.file_name()))?;
            }
        } else if meta.is_file() {
            if let Some(parent) = dst.parent() {
                self.create_dirs(parent)?;
            }
            fs::hard_link(src, dst)?;
        } else {
            return Err(unsupported(src));
        }
        Ok(())
    }

    /// 一時ファイルに書いてから置き換える（管理用のファイル）。
    fn write_meta_file(&self, path: &Path, bytes: &[u8]) -> Result<(), WorkspaceError> {
        let tmp = self.root.join("tmp").join(Self::unique_name());
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

    /// 正本の今の状態を版 `n` として残す。
    fn snapshot(&self, n: u64) -> Result<(), WorkspaceError> {
        let target = self.version_dir(n);
        match self.options.versions {
            VersionBackend::Hardlink => self.link_tree(&self.live_dir(), &target),
            VersionBackend::Btrfs => btrfs(&[
                "subvolume".as_ref(),
                "snapshot".as_ref(),
                "-r".as_ref(),
                self.live_dir().as_os_str(),
                target.as_os_str(),
            ]),
        }
    }

    fn snapshot_live(&self) -> Result<u64, WorkspaceError> {
        let next = self.read_current()? + 1;
        self.snapshot(next)?;
        self.write_meta_file(&self.current_path(), next.to_string().as_bytes())?;
        Ok(next)
    }

    /// コピー方式の変更（作業領域と元の版の差分）。
    fn copy_changes(&self, task: TaskId, meta: &WorkMeta) -> Result<Vec<Pending>, WorkspaceError> {
        let base = self.version_dir(meta.base);
        let view = self.work_dir(task).join("view");
        let mut out = BTreeMap::new();
        for w in &meta.write {
            let before = list_files(&base, w)?;
            let after = list_files(&view, w)?;
            for (path, file) in &after {
                match before.get(path) {
                    None => {
                        out.insert(path.clone(), (ChangeKind::Added, Some(file.clone())));
                    }
                    Some(old) if !same(Some(old), Some(file))? => {
                        out.insert(path.clone(), (ChangeKind::Modified, Some(file.clone())));
                    }
                    Some(_) => {}
                }
            }
            for path in before.keys() {
                if !after.contains_key(path) {
                    out.insert(path.clone(), (ChangeKind::Deleted, None));
                }
            }
        }
        Ok(out.into_iter().map(|(p, (k, s))| (p, k, s)).collect())
    }

    /// OverlayFS 方式の変更（上の層を読む）。
    fn overlay_changes(
        &self,
        task: TaskId,
        meta: &WorkMeta,
    ) -> Result<Vec<Pending>, WorkspaceError> {
        let base = self.version_dir(meta.base);
        let dir = self.work_dir(task);
        let mut out: BTreeMap<String, (ChangeKind, Option<PathBuf>)> = BTreeMap::new();

        fn delete_under(
            base: &Path,
            rel: &str,
            keep: &BTreeSet<String>,
            out: &mut BTreeMap<String, (ChangeKind, Option<PathBuf>)>,
        ) -> Result<(), WorkspaceError> {
            for path in list_files(base, rel)?.into_keys() {
                if !keep.contains(&path) {
                    out.insert(path, (ChangeKind::Deleted, None));
                }
            }
            Ok(())
        }

        /// 上の層を歩く。見つけた通常のファイルの相対パスを返す。
        fn walk(
            upper: &Path,
            rel: &str,
            base: &Path,
            out: &mut BTreeMap<String, (ChangeKind, Option<PathBuf>)>,
        ) -> Result<BTreeSet<String>, WorkspaceError> {
            let mut files = BTreeSet::new();
            for entry in fs::read_dir(upper)? {
                let entry = entry?;
                let path = entry.path();
                let name = entry.file_name();
                let name = name.to_str().ok_or_else(|| unsupported(&path))?;
                let child = join_rel(rel, name);
                let meta = fs::symlink_metadata(&path)?;
                if meta.is_file() {
                    let before = regular(&join(base, &child)).unwrap_or(None);
                    match &before {
                        Some(old) if same(Some(old), Some(&path))? => {}
                        Some(_) => {
                            out.insert(child.clone(), (ChangeKind::Modified, Some(path.clone())));
                        }
                        None => {
                            out.insert(child.clone(), (ChangeKind::Added, Some(path.clone())));
                        }
                    }
                    files.insert(child);
                } else if is_whiteout(&meta) {
                    // 削除の印：元の版にあったファイル（ディレクトリならその中身すべて）を削除する
                    delete_under(base, &child, &BTreeSet::new(), out)?;
                } else if meta.is_dir() {
                    let inner = walk(&path, &child, base, out)?;
                    if is_opaque(&path) {
                        // 下の層を隠すディレクトリ：上の層にないファイルは削除された
                        delete_under(base, &child, &inner, out)?;
                    }
                    files.extend(inner);
                } else {
                    return Err(unsupported(&path));
                }
            }
            Ok(files)
        }

        for (i, w) in meta.write.iter().enumerate() {
            let upper = dir.join("upper").join(i.to_string());
            walk(&upper, w, &base, &mut out)?;
        }
        Ok(out.into_iter().map(|(p, (k, s))| (p, k, s)).collect())
    }

    fn changes(&self, task: TaskId, meta: &WorkMeta) -> Result<Vec<Pending>, WorkspaceError> {
        match meta.view {
            ViewMode::Copy => self.copy_changes(task, meta),
            ViewMode::Overlay => self.overlay_changes(task, meta),
        }
    }
}

impl Workspace for FsStore {
    fn snapshot_level(&self) -> Level {
        match self.options.versions {
            VersionBackend::Hardlink => Level::Weak,
            VersionBackend::Btrfs => Level::Strong,
        }
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
        let labels = self.load_labels()?;
        fs::create_dir_all(&dir)?;

        let mut entries = Vec::new();
        let root = dir.join("view");
        match self.options.view {
            ViewMode::Copy => {
                self.create_dirs(&root)?;
                for r in &read {
                    let (src, dst) = (join(&base_dir, r), join(&root, r));
                    if exists(&src) {
                        self.link_tree(&src, &dst)?;
                    } else {
                        self.create_dirs(&dst)?;
                    }
                    entries.push(ViewEntry {
                        logical: logical(r),
                        source: MountSource::Path(dst),
                        writable: false,
                        labels: labels_for(&labels, r),
                    });
                }
                for w in &write {
                    let (src, dst) = (join(&base_dir, w), join(&root, w));
                    // 読み取り専用の範囲の中にある場合は、ハードリンクを外して本物のコピーに置き換える
                    remove_any(&dst)?;
                    if exists(&src) {
                        self.copy_tree(&src, &dst)?;
                    } else {
                        self.create_dirs(&dst)?;
                    }
                    entries.push(ViewEntry {
                        logical: logical(w),
                        source: MountSource::Path(dst),
                        writable: true,
                        labels: labels_for(&labels, w),
                    });
                }
                // 作業領域の根が消えていたら作り直す（/data 全体を書き込み範囲にした場合）
                self.create_dirs(&root)?;
            }
            ViewMode::Overlay => {
                // 元の版にない範囲は、空のディレクトリを下の層にする
                let empty = dir.join("empty");
                fs::create_dir_all(&empty)?;
                self.own(&empty)?;
                for r in &read {
                    let src = join(&base_dir, r);
                    entries.push(ViewEntry {
                        logical: logical(r),
                        source: MountSource::Path(if exists(&src) { src } else { empty.clone() }),
                        writable: false,
                        labels: labels_for(&labels, r),
                    });
                }
                for (i, w) in write.iter().enumerate() {
                    let lower = join(&base_dir, w);
                    let lower = match fs::symlink_metadata(&lower) {
                        Ok(m) if m.is_dir() => lower,
                        Ok(_) => {
                            return Err(WorkspaceError::InvalidPath(format!(
                                "{}（OverlayFS の書き込み範囲はディレクトリで指定してください）",
                                logical(w)
                            )));
                        }
                        Err(_) => {
                            // 読み取り範囲の中に入れ子にする場合、マウント先が元の版に存在している必要がある
                            if read.iter().any(|r| within(r, w)) {
                                return Err(WorkspaceError::InvalidPath(format!(
                                    "{}（読み取り範囲の中に、元の版にない書き込み範囲は作れません）",
                                    logical(w)
                                )));
                            }
                            empty.clone()
                        }
                    };
                    let upper = dir.join("upper").join(i.to_string());
                    let work = dir.join("ovl").join(i.to_string());
                    fs::create_dir_all(&upper)?;
                    fs::create_dir_all(&work)?;
                    self.own(&upper)?;
                    entries.push(ViewEntry {
                        logical: logical(w),
                        source: MountSource::Overlay { lower, upper, work },
                        writable: true,
                        labels: labels_for(&labels, w),
                    });
                }
            }
        }

        let meta = WorkMeta {
            base,
            read,
            write,
            view: self.options.view,
        };
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
            .map(|(path, kind, _)| Change {
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

        // 3者比較：正本が元の版から変わっていないか、作業領域と同じ内容になっていれば反映してよい
        let mut conflicts = Vec::new();
        for (path, _, after) in &changes {
            let Ok(now) = regular(&live.join(path)) else {
                conflicts.push(logical(path));
                continue;
            };
            let before = regular(&base.join(path))?;
            if !(same(now.as_deref(), before.as_deref())?
                || same(now.as_deref(), after.as_deref())?)
            {
                conflicts.push(logical(path));
            }
        }
        if !conflicts.is_empty() {
            return Ok(CommitOutcome::Conflict { paths: conflicts });
        }

        for (path, kind, after) in &changes {
            let target = live.join(path);
            match (kind, after) {
                (ChangeKind::Added | ChangeKind::Modified, Some(source)) => {
                    if let Some(parent) = target.parent() {
                        self.create_dirs(parent)?;
                    }
                    // 既存のファイルは上書きせず、新しい実体に置き換える（過去の版を壊さない）。
                    // 一時ファイルは正本の中に作る（Btrfs のサブボリュームをまたぐ rename はできないため）
                    let tmp = live.join(format!(".otas-tmp-{}", Self::unique_name()));
                    fs::copy(source, &tmp)?;
                    self.own(&tmp)?;
                    fs::rename(&tmp, &target)?;
                }
                _ => match fs::remove_file(&target) {
                    Ok(()) => {}
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                },
            }
        }

        let mut map = self.load_labels()?;
        for (path, kind, _) in &changes {
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
                .map(|(path, kind, _)| Change {
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
        let staged = self.root.join("tmp").join(Self::unique_name());
        let old = self.root.join("tmp").join(Self::unique_name());
        match self.options.versions {
            VersionBackend::Hardlink => {
                self.link_tree(&src, &staged)?;
                fs::rename(self.live_dir(), &old)?;
                fs::rename(&staged, self.live_dir())?;
                fs::remove_dir_all(&old)?;
            }
            VersionBackend::Btrfs => {
                // 読み取り専用の版から、書き込めるスナップショットを作って正本と入れ替える
                btrfs(&[
                    "subvolume".as_ref(),
                    "snapshot".as_ref(),
                    src.as_os_str(),
                    staged.as_os_str(),
                ])?;
                fs::rename(self.live_dir(), &old)?;
                fs::rename(&staged, self.live_dir())?;
                btrfs(&["subvolume".as_ref(), "delete".as_ref(), old.as_os_str()])?;
            }
        }
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
        store: FsStore,
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
        let store = FsStore::open(&root).unwrap();
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

    fn path_of(entry: &ViewEntry) -> PathBuf {
        match &entry.source {
            MountSource::Path(p) => p.clone(),
            other => panic!("コピー方式なのにパス以外: {other:?}"),
        }
    }

    #[test]
    fn opening_imports_the_current_data_as_version_one() {
        let f = fixture();
        assert_eq!(f.store.current_version().unwrap(), 1);
        assert_eq!(f.store.versions().unwrap(), vec![1]);
        assert_eq!(f.store.snapshot_level(), Level::Weak);
    }

    #[test]
    fn btrfs_requires_overlay() {
        let root = std::env::temp_dir().join(format!("otas-ws-{}", TaskId::new()));
        let r = FsStore::open_with(
            &root,
            StoreOptions {
                view: ViewMode::Copy,
                versions: VersionBackend::Btrfs,
                owner: None,
            },
        );
        assert!(matches!(r, Err(WorkspaceError::Unsupported(_))));
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
        // 一時ファイルは残らない
        assert!(fs::read_dir(f.store.live_dir()).unwrap().all(|e| {
            !e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".otas-tmp")
        }));
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
        let written = v.entries.iter().find(|e| e.writable).unwrap();
        let in_view = fs::metadata(path_of(written).join("notes.md")).unwrap();
        let in_live = fs::metadata(f.store.live_dir().join("projects/a/notes.md")).unwrap();
        // 書き込み範囲のファイルは、正本とは別の実体
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

    #[test]
    fn overlay_views_point_at_the_version_and_the_upper_layer() {
        let root = std::env::temp_dir().join(format!("otas-ws-{}", TaskId::new()));
        fs::create_dir_all(root.join("live/projects/a")).unwrap();
        fs::create_dir_all(root.join("live/mail")).unwrap();
        let store = FsStore::open_with(
            &root,
            StoreOptions {
                view: ViewMode::Overlay,
                ..StoreOptions::default()
            },
        )
        .unwrap();
        let task = TaskId::new();
        let v = store
            .begin(
                task,
                &view(&["/data/mail"], &["/data/projects/a", "/data/new"]),
            )
            .unwrap();
        // 読み取り範囲は版をそのまま見せる（コピーしない）
        assert_eq!(
            v.entries[0].source,
            MountSource::Path(root.join("versions/1/mail"))
        );
        // 書き込み範囲は、版を下の層にした OverlayFS
        match &v.entries[1].source {
            MountSource::Overlay { lower, upper, .. } => {
                assert_eq!(lower, &root.join("versions/1/projects/a"));
                assert!(upper.is_dir());
            }
            other => panic!("{other:?}"),
        }
        // 版にない範囲は空のディレクトリが下の層
        match &v.entries[2].source {
            MountSource::Overlay { lower, .. } => assert!(lower.ends_with("empty")),
            other => panic!("{other:?}"),
        }
        // マウントしていなければ上の層は空なので、変更はない
        assert!(store.diff(task).unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }
}
