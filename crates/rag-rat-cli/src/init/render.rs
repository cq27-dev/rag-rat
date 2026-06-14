use super::*;

pub(crate) fn render_config(plan: &InitPlan) -> String {
    let mut text = String::new();
    text.push_str("[index]\n");
    text.push_str(&format!("root = {}\n", toml_string(&plan.root_value)));
    text.push_str(&format!("database = {}\n\n", toml_string(DEFAULT_DATABASE)));
    text.push_str("[local_ai.embedding]\n");
    text.push_str(&format!("# {}\n", backend_label(plan.backend)));
    text.push_str(&format!("model = {}\n\n", toml_string(plan.backend.as_str())));
    text.push_str("[local_ai.embedding.runtime]\n");
    text.push_str("batch_size = 64\n");
    text.push_str("ort_threads = 4\n");
    text.push_str("omp_threads = 1\n");
    text.push_str("max_embedding_chars = 4000\n\n");
    text.push_str("[target_bindings]\n");
    for language in &plan.languages {
        let dirs = plan.bindings.get(language).cloned().unwrap_or_default();
        text.push_str(&format!("{} = [{}]\n", language.as_str(), quoted_paths(&dirs)));
    }
    text.push('\n');
    text.push_str("[oracle]\n");
    text.push_str(
        "# Background auto-refresh of compiler-grade (SCIP) importance ranking. Needs a \
         language\n# tool on PATH (e.g. rust-analyzer); runs throttled in the MCP server only. \
         Default off.\n",
    );
    text.push_str(&format!("auto_run = {}\n", plan.oracle_auto_run));
    text
}
pub(crate) fn quoted_paths(paths: &[PathBuf]) -> String {
    paths.iter().map(|path| toml_string(&display_rel(path))).collect::<Vec<_>>().join(", ")
}
pub(crate) fn config_root_value(root: &Path, config_path: &Path) -> String {
    let Some(parent) = config_path.parent().filter(|path| !path.as_os_str().is_empty()) else {
        return ".".to_string();
    };
    if config_path.is_absolute() {
        absolute_config_root_value(root, parent)
    } else {
        relative_config_root_value(parent)
    }
}
pub(crate) fn absolute_config_root_value(root: &Path, parent: &Path) -> String {
    if let Ok(relative_parent) = parent.strip_prefix(root) {
        return relative_config_root_value(relative_parent);
    }
    root.display().to_string()
}
pub(crate) fn relative_config_root_value(parent: &Path) -> String {
    let depth = parent.components().filter(normal_component).count();
    if depth == 0 {
        ".".to_string()
    } else {
        std::iter::repeat_n("..", depth).collect::<Vec<_>>().join("/")
    }
}
pub(crate) fn normal_component(component: &std::path::Component<'_>) -> bool {
    matches!(component, std::path::Component::Normal(_))
}
pub(crate) fn toml_string(value: &str) -> String {
    format!("{value:?}")
}
pub(crate) fn display_rel(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if text.is_empty() { ".".to_string() } else { text }
}
pub(crate) fn supported_languages() -> Vec<Language> {
    Language::all().to_vec()
}
