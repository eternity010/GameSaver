use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuntimeStatus {
    pub(crate) is_admin: bool,
    pub(crate) can_use_etw: bool,
    pub(crate) message: String,
}
