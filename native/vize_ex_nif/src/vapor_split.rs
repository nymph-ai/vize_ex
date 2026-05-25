use rustler::{Encoder, Env, Term};
use vize_atelier_vapor::ir::*;

use crate::atoms;
use crate::html_inject::{
    build_elem_to_tag, inject_attr, inject_before_close, parse_tag_tree,
    replace_first_space_in_content,
};
use crate::ir_encoding::{encode_ir_prop, encode_simple_expr};
use crate::term_encoding::nil_term;

fn encode_slot_values<'a>(env: Env<'a>, kind: Term<'a>, values: Term<'a>) -> Term<'a> {
    term_map!(env, {
        atoms::kind() => kind,
        atoms::values() => values,
    })
}

fn encode_slot_value<'a>(
    env: Env<'a>,
    kind: Term<'a>,
    expr: &vize_atelier_core::SimpleExpressionNode,
) -> Term<'a> {
    term_map!(env, {
        atoms::kind() => kind,
        atoms::value() => encode_simple_expr(env, expr),
    })
}

fn encode_split_block<'a, 'b>(
    env: Env<'a>,
    block: &'b BlockIRNode<'b>,
    ir: &'b RootIRNode<'b>,
) -> Term<'a> {
    let (statics, slots) = process_block(env, block, ir);
    let statics_term: Vec<Term<'a>> = statics
        .iter()
        .map(|static_part| static_part.as_str().encode(env))
        .collect();

    term_map!(env, {
        atoms::statics() => statics_term,
        atoms::slots() => slots,
    })
}

fn encode_slot_if_split<'a, 'b>(
    env: Env<'a>,
    if_node: &'b IfIRNode<'b>,
    ir: &'b RootIRNode<'b>,
) -> Term<'a> {
    let negative = match &if_node.negative {
        Some(NegativeBranch::Block(block)) => encode_split_block(env, block, ir),
        Some(NegativeBranch::If(nested)) => encode_slot_if_split(env, nested, ir),
        None => nil_term(env),
    };

    term_map!(env, {
        atoms::kind() => atoms::if_node(),
        atoms::condition() => encode_simple_expr(env, &if_node.condition),
        atoms::positive() => encode_split_block(env, &if_node.positive, ir),
        atoms::negative() => negative,
    })
}

fn encode_slot_for_split<'a, 'b>(
    env: Env<'a>,
    for_node: &'b ForIRNode<'b>,
    ir: &'b RootIRNode<'b>,
) -> Term<'a> {
    term_map!(env, {
        atoms::kind() => atoms::for_node(),
        atoms::source() => encode_simple_expr(env, &for_node.source),
        atoms::value() => for_node
            .value
            .as_ref()
            .map(|value| encode_simple_expr(env, value))
            .unwrap_or_else(|| nil_term(env)),
        atoms::key_prop() => for_node
            .key_prop
            .as_ref()
            .map(|key_prop| encode_simple_expr(env, key_prop))
            .unwrap_or_else(|| nil_term(env)),
        atoms::render() => encode_split_block(env, &for_node.render, ir),
    })
}

fn encode_slot_component<'a>(env: Env<'a>, node: &CreateComponentIRNode) -> Term<'a> {
    let props: Vec<Term<'a>> = node
        .props
        .iter()
        .map(|prop| encode_ir_prop(env, prop))
        .collect();
    let kind_atom = match node.kind {
        ComponentKind::Regular => atoms::regular(),
        ComponentKind::Teleport => atoms::teleport(),
        ComponentKind::KeepAlive => atoms::keep_alive(),
        ComponentKind::Suspense => atoms::suspense(),
        ComponentKind::Dynamic => atoms::dynamic(),
    };

    term_map!(env, {
        atoms::kind() => atoms::create_component(),
        atoms::tag() => node.tag.as_str(),
        atoms::props() => props,
        atoms::value() => kind_atom,
    })
}

// Delimiter for per-slot markers: `\0<slot_index>\0`. Each dynamic slot injects
// a marker carrying its own index. Markers are injected in document order while
// slots are pushed grouped by kind (props, then text, then structural), so the
// index lets `split_on_markers` recover document order and reorder the slots to
// match the static gaps. (Previously markers only encoded a kind, so a row that
// mixed a bound attribute with interpolated text got its values swapped.)
const MARKER_DELIM: &str = "\x00";

fn slot_marker(slot_index: usize) -> String {
    format!("{0}{1}{0}", MARKER_DELIM, slot_index)
}

/// Split `html` on slot markers. Returns the static segments and, for each gap
/// (in document order), the index of the slot that fills it.
fn split_on_markers(html: &str) -> (Vec<String>, Vec<usize>) {
    let mut statics = Vec::new();
    let mut order = Vec::new();
    let mut current = String::new();
    let mut rest = html;

    while let Some(start) = rest.find(MARKER_DELIM) {
        let after = &rest[start + MARKER_DELIM.len()..];
        match after.find(MARKER_DELIM) {
            Some(end_rel) if after[..end_rel].parse::<usize>().is_ok() => {
                let slot_index = after[..end_rel].parse::<usize>().unwrap();
                current.push_str(&rest[..start]);
                statics.push(std::mem::take(&mut current));
                order.push(slot_index);
                rest = &after[end_rel + MARKER_DELIM.len()..];
            }
            _ => {
                // Lone/garbled delimiter — keep it literally and move past it.
                current.push_str(&rest[..start + MARKER_DELIM.len()]);
                rest = after;
            }
        }
    }

    current.push_str(rest);
    statics.push(current);
    (statics, order)
}

#[cfg(test)]
mod tests {
    use super::{slot_marker, split_on_markers};

    #[test]
    fn split_on_markers_recovers_segments_and_document_order() {
        // Markers are emitted out of slot-index order to mimic props (pushed
        // first, higher gap position) interleaved with text (pushed later).
        let html = format!(
            "<div>{}middle{}tail{}</div>",
            slot_marker(2),
            slot_marker(0),
            slot_marker(1)
        );

        let (statics, order) = split_on_markers(&html);

        assert_eq!(
            statics,
            vec![
                "<div>".to_string(),
                "middle".to_string(),
                "tail".to_string(),
                "</div>".to_string()
            ]
        );
        // Document order of the gaps is [slot 2, slot 0, slot 1].
        assert_eq!(order, vec![2, 0, 1]);
    }

    #[test]
    fn split_on_markers_handles_marker_free_html() {
        let (statics, order) = split_on_markers("<div>plain</div>");
        assert_eq!(statics, vec!["<div>plain</div>".to_string()]);
        assert!(order.is_empty());
    }
}

pub(crate) fn process_block<'a, 'b>(
    env: Env<'a>,
    block: &'b BlockIRNode<'b>,
    ir: &'b RootIRNode<'b>,
) -> (Vec<String>, Vec<Term<'a>>) {
    let template_html: String = block
        .returns
        .iter()
        .map(|&elem_id| {
            let template_idx = ir
                .element_template_map
                .get(&elem_id)
                .copied()
                .unwrap_or(elem_id);
            ir.templates
                .get(template_idx)
                .map(|template| template.as_str())
                .unwrap_or("")
        })
        .collect();

    let mut html = template_html;
    let mut tags = parse_tag_tree(&html);
    let elem_to_tag = build_elem_to_tag(&block.returns, &block.operation, &tags);
    let mut slots: Vec<Term<'a>> = Vec::new();

    for op in &block.operation {
        if let OperationNode::SetEvent(event) = op {
            if let Some(&tag_pos) = elem_to_tag.get(&event.element) {
                let event_name = event.key.content.as_str();
                let handler = event
                    .value
                    .as_ref()
                    .map(|value| value.content.as_str())
                    .unwrap_or(event_name);
                let attr = format!(" phx-{}=\"{}\"", event_name, handler);
                inject_attr(&mut html, &mut tags, tag_pos, &attr);
            }
        }
    }

    let all_effects: Vec<_> = block
        .effect
        .iter()
        .flat_map(|effect| effect.operations.iter())
        .collect();

    let mut prop_effects = Vec::new();
    let mut text_effects = Vec::new();
    let mut html_effects = Vec::new();

    for operation in &all_effects {
        match operation {
            OperationNode::SetProp(prop) => prop_effects.push(prop),
            OperationNode::SetText(text) => text_effects.push(text),
            OperationNode::SetHtml(html_node) => html_effects.push(html_node),
            _ => {}
        }
    }

    prop_effects.sort_by_key(|prop| prop.element);

    for prop in &prop_effects {
        if let Some(&tag_pos) = elem_to_tag.get(&prop.element) {
            let attr_name = prop.prop.key.content.as_str();
            let marker = format!(" {}=\"{}\"", attr_name, slot_marker(slots.len()));
            inject_attr(&mut html, &mut tags, tag_pos, &marker);

            let values: Vec<Term<'a>> = prop
                .prop
                .values
                .iter()
                .map(|value| encode_simple_expr(env, value))
                .collect();
            slots.push(encode_slot_values(
                env,
                atoms::set_prop().encode(env),
                values.encode(env),
            ));
        }
    }

    for op in &block.operation {
        if let OperationNode::Directive(dir) = op {
            if let Some(&tag_pos) = elem_to_tag.get(&dir.element) {
                match dir.name.as_str() {
                    "vShow" => {
                        if let Some(vize_atelier_core::ExpressionNode::Simple(simple)) =
                            &dir.dir.exp
                        {
                            let marker = format!(" style=\"{}\"", slot_marker(slots.len()));
                            inject_attr(&mut html, &mut tags, tag_pos, &marker);
                            slots.push(encode_slot_value(env, atoms::v_show().encode(env), simple));
                        }
                    }
                    "model" => {
                        if let Some(vize_atelier_core::ExpressionNode::Simple(simple)) =
                            &dir.dir.exp
                        {
                            let value_marker = format!(" value=\"{}\"", slot_marker(slots.len()));
                            inject_attr(&mut html, &mut tags, tag_pos, &value_marker);
                            slots.push(encode_slot_value(
                                env,
                                atoms::v_model().encode(env),
                                simple,
                            ));
                            let handler_name = format!("{}_changed", simple.content.as_str());
                            let change_attr = format!(" phx-change=\"{}\"", handler_name);
                            inject_attr(&mut html, &mut tags, tag_pos, &change_attr);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    for text in &text_effects {
        if let Some(&tag_pos) = elem_to_tag.get(&text.element) {
            replace_first_space_in_content(&mut html, &mut tags, tag_pos, &slot_marker(slots.len()));
        }

        let values: Vec<Term<'a>> = text
            .values
            .iter()
            .map(|value| encode_simple_expr(env, value))
            .collect();
        slots.push(encode_slot_values(
            env,
            atoms::set_text().encode(env),
            values.encode(env),
        ));
    }

    for html_effect in &html_effects {
        if let Some(&tag_pos) = elem_to_tag.get(&html_effect.element) {
            replace_first_space_in_content(&mut html, &mut tags, tag_pos, &slot_marker(slots.len()));
        }
        slots.push(encode_slot_value(
            env,
            atoms::set_html().encode(env),
            &html_effect.value,
        ));
    }

    for operation in &block.operation {
        match operation {
            OperationNode::If(if_node) => {
                let marker = slot_marker(slots.len());
                if let Some(parent_id) = if_node.parent {
                    if let Some(&tag_pos) = elem_to_tag.get(&parent_id) {
                        inject_before_close(&mut html, &mut tags, tag_pos, &marker);
                    }
                } else {
                    html.push_str(&marker);
                }
                slots.push(encode_slot_if_split(env, if_node, ir));
            }
            OperationNode::For(for_node) => {
                let marker = slot_marker(slots.len());
                if let Some(parent_id) = for_node.parent {
                    if let Some(&tag_pos) = elem_to_tag.get(&parent_id) {
                        inject_before_close(&mut html, &mut tags, tag_pos, &marker);
                    }
                } else {
                    html.push_str(&marker);
                }
                slots.push(encode_slot_for_split(env, for_node, ir));
            }
            OperationNode::CreateComponent(component) => {
                let marker = slot_marker(slots.len());
                if let Some(parent_id) = component.parent {
                    if let Some(&tag_pos) = elem_to_tag.get(&parent_id) {
                        inject_before_close(&mut html, &mut tags, tag_pos, &marker);
                    }
                } else {
                    html.push_str(&marker);
                }
                slots.push(encode_slot_component(env, component));
            }
            _ => {}
        }
    }

    // Markers are injected in document order but slots were pushed grouped by
    // kind; reorder slots to match the document-order gaps from split_on_markers.
    let (statics, order) = split_on_markers(&html);
    let ordered_slots: Vec<Term<'a>> = order
        .into_iter()
        .filter_map(|index| slots.get(index).copied())
        .collect();

    (statics, ordered_slots)
}
