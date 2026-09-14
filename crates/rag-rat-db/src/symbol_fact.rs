//! Stable tokens shared by symbol-fact producers and query predicates.

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr)]
pub enum SymbolFactKind {
    #[strum(serialize = "rust_attr")]
    RustAttr,
}

impl SymbolFactKind {
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }
    pub fn from_db_str(value: &str) -> anyhow::Result<Self> {
        Ok(value.parse()?)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr)]
pub enum SymbolFactValue {
    #[strum(serialize = "uniffi_export")]
    UniffiExport,
}

impl SymbolFactValue {
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }
    pub fn from_db_str(value: &str) -> anyhow::Result<Self> {
        Ok(value.parse()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_fact_tokens_remain_stable() {
        assert_eq!(SymbolFactKind::RustAttr.as_db_str(), "rust_attr");
        assert_eq!(SymbolFactValue::UniffiExport.as_db_str(), "uniffi_export");
        assert_eq!(SymbolFactKind::from_db_str("rust_attr").unwrap(), SymbolFactKind::RustAttr);
        assert_eq!(
            SymbolFactValue::from_db_str("uniffi_export").unwrap(),
            SymbolFactValue::UniffiExport
        );
    }
}
