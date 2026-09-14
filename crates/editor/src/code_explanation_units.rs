use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::ops::Range;

pub const MAX_INPUT_BYTES: usize = 20 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unit {
    pub range: Range<usize>,
    pub owner: Range<usize>,
    pub owner_lines: usize,
    pub first_row: usize,
    pub last_row: usize,
    pub context: String,
    pub commented_rows: Vec<usize>,
}

pub fn first_non_whitespace_column(line: &str) -> usize {
    line.char_indices()
        .find_map(|(column, character)| (!character.is_whitespace()).then_some(column))
        .unwrap_or(0)
}

fn function(node: tree_sitter::Node<'_>) -> bool {
    matches!(
        node.kind(),
        "function_item"
            | "function_definition"
            | "function_declaration"
            | "method_definition"
            | "method_declaration"
            | "arrow_function"
            | "function_expression"
            | "local_function"
            | "function"
    )
}

pub fn units_at(snapshot: &language::BufferSnapshot, row: u32) -> Vec<Unit> {
    let line_start = snapshot.point_to_offset(language::Point::new(row, 0));
    let line_end = snapshot.point_to_offset(language::Point::new(row, snapshot.line_len(row)));
    let line = snapshot
        .text_for_range(line_start..line_end)
        .collect::<String>();
    let content_column = first_non_whitespace_column(&line);
    if content_column == 0 && line.chars().next().is_none_or(char::is_whitespace) {
        return Vec::new();
    }
    let content_offset = line_start + content_column;
    let point = snapshot.offset_to_point(content_offset);
    let Some(mut node) = snapshot.syntax_ancestor(point..point) else {
        return Vec::new();
    };
    let mut owner = node;
    loop {
        if function(node) {
            owner = node;
            break;
        }
        let Some(parent) = node.parent() else {
            break;
        };
        if parent.parent().is_none() {
            break;
        }
        owner = parent;
        node = parent;
    }
    let owner_range = owner.byte_range();
    let owner_lines = owner
        .end_position()
        .row
        .saturating_sub(owner.start_position().row)
        + 1;
    let mut pending = vec![owner];
    let mut result = Vec::new();
    let signature_end = owner
        .child_by_field_name("body")
        .map(|body| body.start_byte())
        .unwrap_or(owner.start_byte());
    let context = snapshot
        .text_for_range(owner.start_byte()..signature_end)
        .collect::<String>()
        .chars()
        .take(1024)
        .collect::<String>();
    let mut visits = 0;
    while let Some(node) = pending.pop() {
        visits += 1;
        if visits > 4096 {
            break;
        }
        if node.kind().contains("comment") || !node.is_named() {
            continue;
        }
        if node.byte_range().len() > MAX_INPUT_BYTES {
            let mut cursor = node.walk();
            let children = node.named_children(&mut cursor).collect::<Vec<_>>();
            pending.extend(children.into_iter().rev());
            continue;
        }
        let first_row = node.start_position().row;
        let mut commented_rows = Vec::new();
        if node.prev_named_sibling().is_some_and(|previous| {
            previous.kind().contains("comment") && previous.end_position().row + 1 >= first_row
        }) {
            commented_rows.push(0);
        }
        let mut descendants = vec![node];
        while let Some(child) = descendants.pop() {
            if child.kind().contains("comment") {
                if let Some(next) = child.next_named_sibling() {
                    if child.end_position().row + 1 >= next.start_position().row {
                        commented_rows.push(next.start_position().row.saturating_sub(first_row));
                    }
                }
            } else {
                let mut cursor = child.walk();
                descendants.extend(child.named_children(&mut cursor));
            }
            if descendants.len() > 4096 {
                break;
            }
        }
        result.push(Unit {
            range: node.byte_range(),
            owner: owner_range.clone(),
            owner_lines,
            first_row,
            last_row: node.end_position().row,
            context: context.clone(),
            commented_rows,
        });
    }
    result
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Annotation {
    pub line: usize,
    pub explanation: String,
}

pub fn parse_annotations(
    output: &str,
    code: &str,
    commented_rows: &[usize],
) -> Result<Vec<Annotation>> {
    let output = output.trim();
    let output = output
        .strip_prefix("```json")
        .or_else(|| output.strip_prefix("```"))
        .and_then(|text| text.strip_suffix("```"))
        .unwrap_or(output)
        .trim();
    let annotations: Vec<Annotation> =
        serde_json::from_str(output).context("讲解返回格式无效，请重试或更换模型")?;
    anyhow::ensure!(annotations.len() <= 128, "讲解条目过多");
    let lines = code.lines().collect::<Vec<_>>();
    let mut seen = std::collections::HashSet::new();
    Ok(annotations
        .into_iter()
        .filter(|annotation| {
            annotation.line > 0
                && annotation.line <= lines.len()
                && lines.get(annotation.line - 1).is_some_and(|line| {
                    let line = line.trim();
                    !line.is_empty()
                        && line.chars().any(|character| character.is_alphanumeric())
                        && !matches!(line, "else" | "else {" | "} else {" | "end")
                        && !line.starts_with("//")
                        && !line.starts_with('#')
                })
                && !commented_rows.contains(&(annotation.line - 1))
                && !annotation.explanation.trim().is_empty()
                && annotation.explanation.len() <= 4096
                && !annotation
                    .explanation
                    .chars()
                    .any(|ch| ch.is_control() && ch != '\n' && ch != '\t')
                && seen.insert(annotation.line)
        })
        .take(16)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[gpui::test]
    fn function_units_preserve_multiline_statements(cx: &mut gpui::App) {
        let code = "fn example() {\n let result = call(\n  1,\n  2,\n );\n}\n";
        let snapshot = language::Buffer::build_snapshot_sync(
            code.into(),
            Some(language::rust_lang()),
            None,
            cx,
        );
        let units = units_at(&snapshot, 2);
        assert_eq!(units.len(), 1);
        assert_eq!(
            snapshot
                .text_for_range(units[0].range.clone())
                .collect::<String>(),
            code.trim_end()
        );
        assert_eq!(units[0].owner_lines, 6);
    }

    #[gpui::test]
    fn rows_select_the_enclosing_function_without_root_overlap(cx: &mut gpui::App) {
        let code = "fn first() {\n    let value = 1;\n}\n\nfn second() {\n    let value = 2;\n}\n";
        let snapshot = language::Buffer::build_snapshot_sync(
            code.into(),
            Some(language::rust_lang()),
            None,
            cx,
        );

        let first = units_at(&snapshot, 1);
        let second = units_at(&snapshot, 5);
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(
            snapshot
                .text_for_range(first[0].range.clone())
                .collect::<String>(),
            "fn first() {\n    let value = 1;\n}"
        );
        assert_eq!(
            snapshot
                .text_for_range(second[0].range.clone())
                .collect::<String>(),
            "fn second() {\n    let value = 2;\n}"
        );
        assert!(units_at(&snapshot, 3).is_empty());
    }

    #[test]
    fn indentation_column_uses_the_first_code_byte() {
        assert_eq!(first_non_whitespace_column("    value"), 4);
        assert_eq!(first_non_whitespace_column("\t\tvalue"), 2);
        assert_eq!(first_non_whitespace_column("  变量"), 2);
        assert_eq!(first_non_whitespace_column(""), 0);
        assert_eq!(first_non_whitespace_column("   "), 0);
    }

    #[gpui::test]
    fn top_level_statements_are_separate_units(cx: &mut gpui::App) {
        let code = "const FIRST: usize = 1;\nconst SECOND: usize = 2;\n";
        let snapshot = language::Buffer::build_snapshot_sync(
            code.into(),
            Some(language::rust_lang()),
            None,
            cx,
        );

        let first = units_at(&snapshot, 0);
        let second = units_at(&snapshot, 1);
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_ne!(first[0].range, second[0].range);
    }

    #[gpui::test]
    fn oversized_functions_split_without_exceeding_budget(cx: &mut gpui::App) {
        let body = "let value = 123456789;\n".repeat(1500);
        let code = format!("fn large() {{\n{body}}}\n");
        let snapshot = language::Buffer::build_snapshot_sync(
            code.into(),
            Some(language::rust_lang()),
            None,
            cx,
        );
        let units = units_at(&snapshot, 2);
        assert!(!units.is_empty());
        assert!(
            units
                .iter()
                .all(|unit| unit.range.len() <= MAX_INPUT_BYTES && unit.owner_lines > 500)
        );
    }

    #[test]
    fn structural_lines_are_not_explained() {
        let code = "fn example() {\n\n}\n);\n// comment\nlet result = call();";
        let output = (1..=6)
            .map(|line| Annotation {
                line,
                explanation: "解释".into(),
            })
            .collect::<Vec<_>>();
        let parsed =
            parse_annotations(&serde_json::to_string(&output).unwrap(), code, &[]).unwrap();
        assert_eq!(
            parsed
                .iter()
                .map(|annotation| annotation.line)
                .collect::<Vec<_>>(),
            vec![1, 6]
        );
    }

    #[test]
    fn annotations_are_bounded_and_comment_aware() {
        let result = parse_annotations(r#"[{"line":1,"explanation":"existing"},{"line":2,"explanation":"valid"},{"line":2,"explanation":"duplicate"},{"line":0,"explanation":"invalid"},{"line":9,"explanation":"outside"}]"#, "one\ntwo", &[0]).unwrap();
        assert_eq!(
            result,
            vec![Annotation {
                line: 2,
                explanation: "valid".into()
            }]
        );
        assert!(parse_annotations("not json", "code", &[]).is_err());
    }
}
