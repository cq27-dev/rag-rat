use std::path::Path;

use crate::index::parser;
use crate::language::Language;

#[derive(Debug, Clone)]
pub struct SymbolFact {
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub qualified_name: String,
    /// Semantic scope path (see [`parser::ParsedSymbol::scope_path`]) — the resolution key that
    /// aligns with edges' source-derived `target_qualified_name`.
    pub scope_path: String,
    pub kind: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub signature: Option<String>,
    pub docs: Option<String>,
    /// Test code (see [`parser::ParsedSymbol::is_test`]) — persisted so clone detection can keep
    /// tests out of the corpus.
    pub is_test: bool,
    pub facts: Vec<SymbolFact>,
}

pub fn symbols_for_file(path: &Path, language: Language, text: &str) -> Vec<Symbol> {
    from_parsed(&parser::parse_symbols(path, language, text).unwrap_or_default())
}

/// Convert parser symbols into index symbols. Shared by `symbols_for_file` (inline path) and the
/// full-rebuild prepare phase, which already has the parsed symbols from the single shared parse.
pub fn from_parsed(parsed: &[parser::ParsedSymbol]) -> Vec<Symbol> {
    parsed
        .iter()
        .map(|symbol| Symbol {
            name: symbol.name.clone(),
            qualified_name: symbol.qualified_name.clone(),
            scope_path: symbol.scope_path.clone(),
            kind: symbol.kind.clone(),
            start_byte: symbol.start_byte,
            end_byte: symbol.end_byte,
            start_line: symbol.start_line,
            end_line: symbol.end_line,
            signature: symbol.signature.clone(),
            docs: symbol.docs.clone(),
            is_test: symbol.is_test,
            facts: symbol
                .facts
                .iter()
                .map(|fact| SymbolFact { kind: fact.kind.clone(), value: fact.value.clone() })
                .collect(),
        })
        .collect()
}
