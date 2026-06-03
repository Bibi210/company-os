use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use companyos_config::watcher::{self, ConfigChangeKind};
use companyos_config::{Diagnostic, constants};
use companyos_validation::{ArtifactValidator, SchemaRegistry};
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_handler, tool_router,
};
use serde::Deserialize;
use tokio::sync::RwLock;

const C: &str = constants::COMPONENT_YAML_VALIDATOR;

#[derive(Clone)]
struct YamlValidatorServer {
    validator: Arc<RwLock<ArtifactValidator>>,
    schemas_dir: String,
    tool_router: ToolRouter<Self>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ValidateYamlParams {
    #[schemars(description = "YAML content to validate")]
    yaml: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ValidateFileParams {
    #[schemars(description = "Path to the YAML file to validate")]
    path: String,
}

fn ok(json: String) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

fn err(msg: String) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::error(vec![Content::text(msg)]))
}

#[tool_router]
impl YamlValidatorServer {
    fn new(validator: Arc<RwLock<ArtifactValidator>>, schemas_dir: String) -> Self {
        Self {
            validator,
            schemas_dir,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Validate a YAML string against its JSON Schema (kind auto-detected from the YAML content)"
    )]
    async fn validate_yaml(
        &self,
        params: Parameters<ValidateYamlParams>,
    ) -> Result<CallToolResult, McpError> {
        let params = params.0;
        let validator = self.validator.read().await;
        match validator.validate_yaml_str(&params.yaml) {
            Ok(report) => ok(serde_json::to_string_pretty(&serde_json::json!({
                "kind": report.kind,
                "id": report.id,
                "is_valid": report.is_valid,
                "errors": report.errors,
                "warnings": report.warnings,
            }))
            .unwrap_or_default()),
            Err(e) => err(Diagnostic::error(C, "YAML validation failed")
                .with_context("validate_yaml (inline content)")
                .with_reason(format!("{e}"))
                .with_fix("Check that the YAML has valid api_version, kind, and metadata.id fields")
                .to_string()),
        }
    }

    #[tool(description = "Validate a YAML file at the given path against its JSON Schema")]
    async fn validate_file(
        &self,
        params: Parameters<ValidateFileParams>,
    ) -> Result<CallToolResult, McpError> {
        let params = params.0;
        let validator = self.validator.read().await;
        let path = std::path::Path::new(&params.path);
        match validator.validate_file(path) {
            Ok(report) => ok(serde_json::to_string_pretty(&serde_json::json!({
                "kind": report.kind,
                "id": report.id,
                "is_valid": report.is_valid,
                "errors": report.errors,
                "warnings": report.warnings,
            }))
            .unwrap_or_default()),
            Err(e) => err(Diagnostic::error(C, "File validation failed")
                .with_context(format!("validate_file(path={})", params.path))
                .with_reason(format!("{e}"))
                .with_fix(
                    "Ensure the file exists, is valid YAML, and has api_version/kind/metadata.id",
                )
                .to_string()),
        }
    }

    #[tool(description = "Reload JSON Schemas from disk (picks up new or modified schemas)")]
    async fn reload_schemas(&self) -> Result<CallToolResult, McpError> {
        match SchemaRegistry::load(&self.schemas_dir) {
            Ok(registry) => {
                let kinds: Vec<_> = registry.kinds().iter().map(|k| k.as_str()).collect();
                let new_validator = ArtifactValidator::new(registry);
                *self.validator.write().await = new_validator;
                ok(serde_json::to_string_pretty(&serde_json::json!({
                    "reloaded": true,
                    "schema_count": kinds.len(),
                    "kinds": kinds
                }))
                .unwrap_or_default())
            }
            Err(e) => err(Diagnostic::error(C, "Failed to reload schemas")
                .with_context(format!("reload_schemas(dir={})", self.schemas_dir))
                .with_reason(format!("{e}"))
                .with_fix("Check that the schemas/ directory exists and contains valid .schema.json files")
                .to_string()),
        }
    }
}

#[tool_handler]
impl ServerHandler for YamlValidatorServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "companyos-yaml-validator",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions("Validates YAML artifacts against their JSON Schemas. Read-only.")
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(|s| s.as_str()) {
        Some("--batch") => {
            let dir = args.get(2).map(|s| s.as_str()).unwrap_or("company/");
            run_batch(dir)
        }
        Some("--file") => {
            let path = args
                .get(2)
                .ok_or_else(|| anyhow::anyhow!("usage: --file <path>"))?;
            run_single_file(path)
        }
        _ => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(run_server()),
    }
}

async fn run_server() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter("info")
        .init();

    let root = std::env::var(constants::ENV_COMPANYOS_ROOT).unwrap_or_else(|_| ".".into());
    let schemas_dir = format!("{root}/{}", constants::SCHEMAS_DIR);

    let registry = SchemaRegistry::load(&schemas_dir)?;
    let validator = Arc::new(RwLock::new(ArtifactValidator::new(registry)));
    let server = YamlValidatorServer::new(validator.clone(), schemas_dir.clone());

    // RFC 062ebaa8 — shared flag set right before the service exits so
    // the `WatcherGuard` Drop impl emits info!, not error!, on a clean
    // shutdown. Cf. diagnostic 9534dd33.
    let is_shutting_down = Arc::new(AtomicBool::new(false));

    // File watcher: auto-reload schemas on disk changes.
    //
    // RFC 062ebaa8 — the `handle` is destructured here (NOT passed by
    // move into the closure) so that the `_guard` field can be captured
    // explicitly inside the `async move` block via `let _keep = _guard;`.
    // Without this, Rust 2024 disjoint capture for async closures would
    // leave `_guard` outside the future, dropping the RecommendedWatcher
    // at the end of this match arm and killing the inotify fd (cf.
    // diagnostic 9534dd33).
    match watcher::spawn_watcher(&root, Duration::from_millis(500), is_shutting_down.clone()) {
        Ok(handle) => {
            let watcher::FileWatcherHandle { mut rx, _guard } = handle;
            let validator = validator.clone();
            let schemas_dir = schemas_dir.clone();
            tokio::spawn(async move {
                // MUST keep the guard alive for as long as `rx` is
                // consumed. DO NOT remove this binding even if it looks
                // unused — see diagnostic 9534dd33.
                let _keep_guard = _guard;

                while let Some(change) = rx.recv().await {
                    if matches!(change, ConfigChangeKind::Schemas) {
                        match SchemaRegistry::load(&schemas_dir) {
                            Ok(registry) => {
                                let count = registry.kinds().len();
                                *validator.write().await = ArtifactValidator::new(registry);
                                tracing::info!("Auto-reloaded {count} schema(s)");
                            }
                            Err(e) => tracing::warn!("Auto-reload schemas failed: {e}"),
                        }
                    }
                }
            });
        }
        Err(e) => {
            tracing::warn!("File watcher unavailable, manual reload_schemas still works: {e}");
        }
    }

    let (stdin, stdout) = rmcp::transport::io::stdio();
    let service = server.serve((stdin, stdout)).await?;
    let result = service.waiting().await;
    // Service finished (stdin closed, transport error, etc.). Mark the
    // shutdown flag BEFORE returning so any drop of the watcher guard
    // that races us logs info!, not error!.
    is_shutting_down.store(true, Ordering::Release);
    result?;

    Ok(())
}

fn run_single_file(path: &str) -> anyhow::Result<()> {
    let root = std::env::var(constants::ENV_COMPANYOS_ROOT).unwrap_or_else(|_| ".".into());
    let schemas_dir = format!("{root}/{}", constants::SCHEMAS_DIR);

    let registry = SchemaRegistry::load(&schemas_dir)?;
    let validator = ArtifactValidator::new(registry);

    match validator.validate_file(std::path::Path::new(path)) {
        Ok(report) if report.is_valid => {
            println!(
                "{}",
                Diagnostic::info(C, format!("Valid: {path}"))
                    .with_context(format!("validate_file(path={path})"))
            );
            Ok(())
        }
        Ok(report) => {
            let errors = report.errors.join("; ");
            eprintln!(
                "{}",
                Diagnostic::error(C, format!("Schema validation failed: {path}"))
                    .with_context(format!("validate_file(path={path})"))
                    .with_reason(errors)
                    .with_fix("Fix the YAML to match its JSON Schema, then retry")
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!(
                "{}",
                Diagnostic::error(C, format!("Cannot validate: {path}"))
                    .with_context(format!("validate_file(path={path})"))
                    .with_reason(format!("{e}"))
                    .with_fix(
                        "Ensure the file exists, is valid YAML, and has api_version/kind/metadata.id",
                    )
            );
            std::process::exit(1);
        }
    }
}

fn run_batch(dir: &str) -> anyhow::Result<()> {
    let root = std::env::var(constants::ENV_COMPANYOS_ROOT).unwrap_or_else(|_| ".".into());
    let schemas_dir = format!("{root}/{}", constants::SCHEMAS_DIR);

    let registry = SchemaRegistry::load(&schemas_dir)?;
    let validator = ArtifactValidator::new(registry);

    let results = validator.validate_dir(std::path::Path::new(dir));
    let mut has_errors = false;

    for (path, result) in &results {
        let p = path.display();
        match result {
            Ok(report) if report.is_valid => {
                println!(
                    "{}",
                    Diagnostic::info(C, format!("Valid: {p}"))
                        .with_context(format!("validate_file(path={p})"))
                );
            }
            Ok(report) => {
                has_errors = true;
                let errors = report.errors.join("; ");
                println!(
                    "{}",
                    Diagnostic::error(C, format!("Schema validation failed: {p}"))
                        .with_context(format!("validate_file(path={p})"))
                        .with_reason(errors)
                        .with_fix("Fix the YAML to match its JSON Schema, then retry")
                );
            }
            Err(e) => {
                // Skip non-artifact YAML files (e.g., auto-generated) in batch mode
                eprintln!(
                    "{}",
                    Diagnostic::warning(C, format!("Skipped: {p}"))
                        .with_context(format!("validate_file(path={p})"))
                        .with_reason(format!("{e}"))
                );
            }
        }
    }

    println!("\n{} files checked.", results.len());

    if has_errors {
        std::process::exit(1);
    }
    Ok(())
}
