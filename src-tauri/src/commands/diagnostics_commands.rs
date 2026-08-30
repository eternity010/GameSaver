use crate::logging;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrontendErrorReport {
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub stack: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default)]
    pub column: Option<u32>,
}

#[tauri::command]
pub fn report_frontend_error(error: FrontendErrorReport) -> Result<(), String> {
    logging::error(format!(
        "前端异常 source={} message={} stack={} location={}:{}:{}",
        error.source,
        error.message,
        error.stack.unwrap_or_default(),
        error.url.unwrap_or_default(),
        error.line.unwrap_or_default(),
        error.column.unwrap_or_default(),
    ));
    Ok(())
}
