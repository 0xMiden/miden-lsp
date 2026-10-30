use std::{ops::Range as StdRange, sync::Arc};

use miden_assembly_syntax::debuginfo::{DefaultSourceManager, SourceLanguage, SourceManager, Uri};
use miden_assembly_syntax_cst::{Parse, parse_source_file};
use tower_lsp::lsp_types::{
    Diagnostic, DiagnosticSeverity, Position, Range, TextDocumentContentChangeEvent, Url,
};

#[derive(Debug)]
pub struct TextDocument {
    uri: Url,
    version: i32,
    text: String,
    line_offsets: Vec<usize>,
    tree: Parse,
    syntax_diagnostics: Vec<Diagnostic>,
}

impl TextDocument {
    pub fn new(uri: Url, version: i32, text: String) -> Result<Self, String> {
        let (tree, syntax_diagnostics) = parse_document(&uri, &text)?;
        let line_offsets = compute_line_offsets(&text);
        Ok(Self {
            uri,
            version,
            text,
            line_offsets,
            tree,
            syntax_diagnostics,
        })
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
        let mut text = self.text.clone();
        let mut line_offsets = self.line_offsets.clone();
        for change in changes {
            if let Some(range) = change.range {
                let start = position_to_offset(&text, &line_offsets, range.start)?;
                let end = position_to_offset(&text, &line_offsets, range.end)?;
                if start > end {
                    return Err("range end precedes its start".into());
                }
                text.replace_range(start..end, &change.text);
            } else {
                text = change.text;
            }
            line_offsets = compute_line_offsets(&text);
        }

        let (tree, diagnostics) = parse_document(&self.uri, &text)?;
        self.text = text;
        self.line_offsets = line_offsets;
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
        if start > end {
            return Err("range end precedes its start".into());
        }
        Ok(start..end)
    }
}

pub fn parse_text(text: &str) -> Result<(Parse, Vec<Diagnostic>), String> {
    parse_document(&Url::parse("untitled:miden.masm").unwrap(), text)
}

pub fn parse_document(uri: &Url, text: &str) -> Result<(Parse, Vec<Diagnostic>), String> {
    let manager = Arc::new(DefaultSourceManager::default());
    let source = manager.load(SourceLanguage::Masm, Uri::new(uri.as_str()), text.to_owned());
    let tree = parse_source_file(source);
    let line_offsets = compute_line_offsets(text);
    let diagnostics = tree
        .diagnostics()
        .iter()
        .map(|error| {
            let bounds = error
                .labels
                .as_ref()
                .and_then(|labels| labels.first())
                .map(|label| label.offset()..label.offset() + label.len())
                .unwrap_or(0..0);
            Diagnostic {
                range: byte_range_to_lsp_range(text, &line_offsets, bounds),
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("miden-assembler".into()),
                message: error
                    .labels
                    .as_ref()
                    .and_then(|labels| labels.first())
                    .and_then(|label| label.label())
                    .map(|label| format!("{}: {label}", error.message))
                    .unwrap_or_else(|| error.message.clone()),
                ..Diagnostic::default()
            }
        })
        .collect();
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
    let target_units =
        usize::try_from(position.character).map_err(|_| "character offset overflow".to_string())?;

    let mut utf16_units = 0usize;
    for (offset, ch) in line_text.char_indices() {
        if utf16_units >= target_units {
            return Ok(line_start + offset);
        }
        if utf16_units + ch.len_utf16() > target_units {
            return Err("position splits a UTF-16 surrogate pair".into());
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
    let mut bounded = offset.min(text.len());
    while !text.is_char_boundary(bounded) {
        bounded -= 1;
    }
    let line = match line_offsets.binary_search(&bounded) {
        Ok(index) => index,
        Err(index) => index.saturating_sub(1),
    };
    let line_start = line_offsets.get(line).copied().unwrap_or(0);
    let line_slice = &text[line_start..bounded];
    let character = line_slice.encode_utf16().count();
    Position::new(line as u32, character as u32)
}

pub fn byte_range_to_lsp_range(
    text: &str,
    line_offsets: &[usize],
    range: StdRange<usize>,
) -> Range {
    Range::new(
        offset_to_position(text, line_offsets, range.start),
        offset_to_position(text, line_offsets, range.end),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembler_cst_preserves_trivia_and_the_source_uri() {
        let uri = Url::parse("file:///tmp/lossless.masm").unwrap();
        let text = "#! docs\npub proc helper # comment\n    push.1 drop\nend\n";
        let document = TextDocument::new(uri.clone(), 1, text.to_string()).unwrap();
        assert!(document.syntax_diagnostics().is_empty());
        assert_eq!(document.tree.syntax().text().to_string(), text);
        assert_eq!(document.tree.source().uri().as_str(), uri.as_str());
    }

    #[test]
    fn assembler_cst_reports_incomplete_blocks_without_discarding_the_tree() {
        let text = "pub proc helper\n    push.1\n";
        let (tree, diagnostics) = parse_text(text).unwrap();
        assert_eq!(tree.syntax().text().to_string(), text);
        assert!(!diagnostics.is_empty());
        assert!(diagnostics.iter().any(|diagnostic| diagnostic.message.contains("end")));
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.source.as_deref() == Some("miden-assembler"))
        );
    }

    #[test]
    fn changes_use_utf16_offsets_and_reject_split_surrogates() {
        let uri = Url::parse("file:///tmp/unicode.masm").unwrap();
        let mut document =
            TextDocument::new(uri, 1, "# 😀 comment\npub proc foo\nnop\nend\n".to_string())
                .unwrap();
        assert!(document.position_to_offset(Position::new(0, 3)).is_err());
        assert_eq!(document.position_to_offset(Position::new(0, 4)).unwrap(), "# 😀".len());
        document
            .apply_changes(
                2,
                vec![TextDocumentContentChangeEvent {
                    range: Some(Range::new(Position::new(0, 2), Position::new(0, 4))),
                    range_length: None,
                    text: "ok".into(),
                }],
            )
            .unwrap();
        assert!(document.text().starts_with("# ok comment"));
        assert_eq!(document.tree.syntax().text().to_string(), document.text());
    }

    #[test]
    fn invalid_change_batches_do_not_leave_text_and_cst_out_of_sync() {
        let uri = Url::parse("file:///tmp/changes.masm").unwrap();
        let original = "pub proc foo\nnop\nend\n";
        let mut document = TextDocument::new(uri, 1, original.into()).unwrap();
        let changes = vec![
            TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "begin end".into(),
            },
            TextDocumentContentChangeEvent {
                range: Some(Range::new(Position::new(99, 0), Position::new(99, 1))),
                range_length: None,
                text: "bad".into(),
            },
        ];
        assert!(document.apply_changes(2, changes).is_err());
        assert_eq!(document.text(), original);
        assert_eq!(document.tree.syntax().text().to_string(), original);
        assert_eq!(document.version(), 1);
        assert!(
            document
                .range_to_offsets(Range::new(Position::new(0, 5), Position::new(0, 1)))
                .is_err()
        );
    }

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
