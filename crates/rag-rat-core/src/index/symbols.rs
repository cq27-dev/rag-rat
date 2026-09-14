//! Index symbols retain the parser's representation without copying its symbol table.

use std::path::Path;

use rag_rat_base::language::Language;

use super::parser;

pub type Symbol = parser::ParsedSymbol;
pub type SymbolFact = parser::ParsedSymbolFact;

pub fn symbols_for_file(path: &Path, language: Language, text: &str) -> Vec<Symbol> {
    parser::parse_symbols(path, language, text).unwrap_or_default()
}
