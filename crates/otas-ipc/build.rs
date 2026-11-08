//! Cap'n Proto スキーマから Rust のコードを生成する。

fn main() {
    println!("cargo:rerun-if-changed=schema/otas.capnp");
    capnpc::CompilerCommand::new()
        .src_prefix("schema")
        .file("schema/otas.capnp")
        .run()
        .expect("Cap'n Proto スキーマのコンパイルに失敗しました（capnp コマンドが必要です）");
}
