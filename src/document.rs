use std::ops::Range as StdRange;

use tower_lsp::lsp_types::{
    Diagnostic, DiagnosticSeverity, Position, Range, TextDocumentContentChangeEvent, Url,
};
use tree_sitter::{Parser, Point, Tree};

#[derive(Debug)]
pub struct TextDocument {
    uri: Url,
    version: i32,
    text: String,
    line_offsets: Vec<usize>,
    tree: Tree,
    syntax_diagnostics: Vec<Diagnostic>,
}

impl TextDocument {
    pub fn new(uri: Url, version: i32, text: String) -> Result<Self, String> {
        let (tree, syntax_diagnostics) = parse_text(&text)?;
        let line_offsets = compute_line_offsets(&text);
        Ok(Self { uri, version, text, line_offsets, tree, syntax_diagnostics })
    }

    pub fn uri(&self) -> &Url {
        &self.uri
    }

    pub fn version(&self) -> i32 {
        self.version
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn syntax_diagnostics(&self) -> &[Diagnostic] {
        &self.syntax_diagnostics
    }

    pub fn apply_changes(
        &mut self,
        version: i32,
        changes: Vec<TextDocumentContentChangeEvent>,
    ) -> Result<(), String> {
        for change in changes {
            if let Some(range) = change.range {
                let offsets = self.range_to_offsets(range)?;
                self.text.replace_range(offsets, &change.text);
            } else {
                self.text = change.text;
            }
            self.line_offsets = compute_line_offsets(&self.text);
        }

        let (tree, diagnostics) = parse_text(&self.text)?;
        self.tree = tree;
        self.syntax_diagnostics = diagnostics;
        self.version = version;
        Ok(())
    }

    pub fn position_to_offset(&self, position: Position) -> Result<usize, String> {
        position_to_offset(&self.text, &self.line_offsets, position)
    }

    pub fn offset_to_position(&self, offset: usize) -> Position {
        offset_to_position(&self.text, &self.line_offsets, offset)
    }

    pub fn range_to_offsets(&self, range: Range) -> Result<StdRange<usize>, String> {
        let start = self.position_to_offset(range.start)?;
        let end = self.position_to_offset(range.end)?;
        Ok(start..end)
    }
}

pub fn parse_text(text: &str) -> Result<(Tree, Vec<Diagnostic>), String> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_masm::LANGUAGE.into())
        .map_err(|error| format!("failed to load tree-sitter-masm language: {error}"))?;

    let tree = parser
        .parse(text, None)
        .ok_or_else(|| "tree-sitter failed to parse document".to_string())?;
    let diagnostics = collect_syntax_diagnostics(text, &tree);
    Ok((tree, diagnostics))
}

pub fn compute_line_offsets(text: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            offsets.push(index + 1);
        }
    }
    offsets
}

pub fn position_to_offset(
    text: &str,
    line_offsets: &[usize],
    position: Position,
) -> Result<usize, String> {
    let line = usize::try_from(position.line).map_err(|_| "line number overflow".to_string())?;
    let Some(&line_start) = line_offsets.get(line) else {
        return Err(format!("line {} out of bounds", position.line));
    };

    let line_end = line_offsets.get(line + 1).copied().unwrap_or(text.len());
    let line_text = &text[line_start..line_end];
    let target_units = usize::try_from(position.character)
        .map_err(|_| "character offset overflow".to_string())?;

    let mut utf16_units = 0usize;
    for (offset, ch) in line_text.char_indices() {
        if utf16_units >= target_units {
            return Ok(line_start + offset);
        }
        utf16_units += ch.len_utf16();
    }

    if utf16_units == target_units {
        Ok(line_end)
    } else {
        Err(format!(
            "character offset {} out of bounds for line {}",
            position.character, position.line
        ))
    }
}

pub fn offset_to_position(text: &str, line_offsets: &[usize], offset: usize) -> Position {
    let bounded = offset.min(text.len());
    let line = match line_offsets.binary_search(&bounded) {
        Ok(index) => index,
        Err(index) => index.saturating_sub(1),
    };
    let line_start = line_offsets.get(line).copied().unwrap_or(0);
    let line_slice = &text[line_start..bounded];
    let character = line_slice.encode_utf16().count();
    Position::new(line as u32, character as u32)
}

pub fn point_to_position(point: Point) -> Position {
    Position::new(point.row as u32, point.column as u32)
}

pub fn byte_range_to_lsp_range(text: &str, line_offsets: &[usize], range: StdRange<usize>) -> Range {
    Range::new(
        offset_to_position(text, line_offsets, range.start),
        offset_to_position(text, line_offsets, range.end),
    )
}

fn collect_syntax_diagnostics(text: &str, tree: &Tree) -> Vec<Diagnostic> {
    let line_offsets = compute_line_offsets(text);
    let mut diagnostics = Vec::new();
    let mut cursor = tree.walk();
    let mut pending = vec![tree.root_node()];

    while let Some(node) = pending.pop() {
        if node.is_error() || node.is_missing() {
            let start = node.start_byte();
            let mut end = node.end_byte();
            if end <= start {
                end = start.saturating_add(1).min(text.len());
            }
            diagnostics.push(Diagnostic {
                range: byte_range_to_lsp_range(text, &line_offsets, start..end),
                severity: Some(DiagnosticSeverity::ERROR),
                message: if node.is_missing() {
                    format!("missing {}", node.kind())
                } else {
                    "syntax error".to_string()
                },
                ..Diagnostic::default()
            });
        }

        for child in node.named_children(&mut cursor) {
            pending.push(child);
        }
    }

    diagnostics.sort_by_key(|diagnostic| {
        (diagnostic.range.start.line, diagnostic.range.start.character)
    });
    diagnostics.dedup_by(|left, right| left.range == right.range && left.message == right.message);
    diagnostics
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_incremental_changes_against_utf16_positions() {
        let uri = Url::parse("file:///tmp/test.masm").unwrap();
        let mut document =
            TextDocument::new(uri, 1, "const FOO=1\nproc foo\n    push.1\nend\n".to_string())
                .unwrap();

        document
            .apply_changes(
                2,
                vec![TextDocumentContentChangeEvent {
                    range: Some(Range::new(Position::new(1, 5), Position::new(1, 8))),
                    range_length: None,
                    text: "bar".to_string(),
                }],
            )
            .unwrap();

        assert!(document.text().contains("proc bar"));
    }
}
