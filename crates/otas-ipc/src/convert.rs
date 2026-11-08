//! Rust の型とスキーマの型の相互変換。

use crate::otas_capnp;
use otas_ports::Role;
use otas_types::{Label, LabelSet};

pub(crate) fn label_to_wire(label: Label) -> otas_capnp::Label {
    match label {
        Label::Untrusted => otas_capnp::Label::Untrusted,
        Label::Private => otas_capnp::Label::Private,
        Label::Secret => otas_capnp::Label::Secret,
    }
}

pub(crate) fn label_from_wire(label: otas_capnp::Label) -> Label {
    match label {
        otas_capnp::Label::Untrusted => Label::Untrusted,
        otas_capnp::Label::Private => Label::Private,
        otas_capnp::Label::Secret => Label::Secret,
    }
}

pub(crate) fn role_to_wire(role: Role) -> otas_capnp::Role {
    match role {
        Role::System => otas_capnp::Role::System,
        Role::User => otas_capnp::Role::User,
        Role::Assistant => otas_capnp::Role::Assistant,
    }
}

pub(crate) fn role_from_wire(role: otas_capnp::Role) -> Role {
    match role {
        otas_capnp::Role::System => Role::System,
        otas_capnp::Role::User => Role::User,
        otas_capnp::Role::Assistant => Role::Assistant,
    }
}

pub(crate) fn read_labels(
    list: capnp::enum_list::Reader<'_, otas_capnp::Label>,
) -> capnp::Result<LabelSet> {
    let mut labels = Vec::with_capacity(list.len() as usize);
    for item in list.iter() {
        let label = item.map_err(|e| capnp::Error::failed(format!("未知のラベルです: {e:?}")))?;
        labels.push(label_from_wire(label));
    }
    Ok(LabelSet::of(labels))
}

pub(crate) fn write_labels(
    mut list: capnp::enum_list::Builder<'_, otas_capnp::Label>,
    labels: &LabelSet,
) {
    for (i, label) in labels.iter().enumerate() {
        list.set(i as u32, label_to_wire(label));
    }
}

pub(crate) fn text(reader: capnp::text::Reader<'_>) -> capnp::Result<String> {
    reader
        .to_string()
        .map_err(|e| capnp::Error::failed(format!("文字列が UTF-8 ではありません: {e}")))
}
