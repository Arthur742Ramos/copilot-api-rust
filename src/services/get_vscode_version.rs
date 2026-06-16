/// Mirrors services/get-vscode-version.ts. The version string baked into Copilot
/// editor headers. Intentionally a hard-coded constant (the async shape is
/// vestigial in the source); must match exactly for header fingerprinting.
pub const VSCODE_VERSION_FALLBACK: &str = "1.124.2";

pub async fn get_vscode_version() -> String {
    VSCODE_VERSION_FALLBACK.to_string()
}
