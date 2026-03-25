use chorograph_plugin_sdk_rust::prelude::*;
use once_cell::sync::Lazy;
use regex::Regex;

// ---------------------------------------------------------------------------
// Compiled regexes
// ---------------------------------------------------------------------------

/// Matches <TargetFramework>net9.0</TargetFramework>
static RE_TARGET_FRAMEWORK: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<TargetFramework>(.*?)</TargetFramework>").unwrap());

/// Matches any builder.Add*("name") or builder.Add*<Type>("name") call.
/// Captures:
///   1 — the Add* suffix (e.g. "Project", "Postgres", "Redis", "RabbitMQ", "NpmApp", ...)
///   2 — the generic type argument, if present (e.g. "MyApp_Api") — optional capture group
///   3 — the resource name string argument (e.g. "api", "db", "cache")
static RE_ADD_RESOURCE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"builder\s*\.\s*Add([A-Za-z][A-Za-z0-9]*)\s*(?:<\s*(?:[A-Za-z0-9_.]+\.)?([A-Za-z0-9_]+)\s*>)?\s*\(\s*"([^"]+)""#,
    )
    .unwrap()
});

/// Matches AddOpenApi() or AddOpenApi("docName") or AddOpenApiDefaults(...)
/// Captures optional document name (group 1).
static RE_ADD_OPENAPI: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"AddOpenApi(?:Defaults)?\s*\(\s*(?:"([^"]*)")?"#).unwrap());

/// Matches MapOpenApi() or MapOpenApi("docName") or MapOpenApi("/path/{documentName}/...")
/// Captures optional document name or path (group 1).
static RE_MAP_OPENAPI: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"MapOpenApi\s*\(\s*(?:"([^"]*)")?"#).unwrap());

/// Matches AddSwaggerGen() (Swashbuckle)
static RE_ADD_SWAGGER: Lazy<Regex> = Lazy::new(|| Regex::new(r"AddSwaggerGen\s*\(").unwrap());

// ---------------------------------------------------------------------------
// Aspire AppHost detection signals
// ---------------------------------------------------------------------------

fn is_aspire_apphost(csproj: &str) -> bool {
    // Newer SDK-style AppHost
    csproj.contains("Sdk=\"Aspire.AppHost.Sdk\"")
        || csproj.contains("Sdk='Aspire.AppHost.Sdk'")
        || csproj.contains("<Sdk Name=\"Aspire.AppHost.Sdk\"")
        || csproj.contains("<Sdk Name='Aspire.AppHost.Sdk'")
        // Package reference styles (older / mixed)
        || csproj.contains("Aspire.Hosting.AppHost")
        || csproj.contains("Aspire.Hosting\"")
        || csproj.contains("Aspire.Hosting'")
}

// ---------------------------------------------------------------------------
// Plugin entry points
// ---------------------------------------------------------------------------

#[chorograph_plugin]
pub fn init() {
    log!("Aspire Plugin Loaded");
}

#[chorograph_plugin]
pub fn identify_project(root: String, files: Vec<String>) -> Option<ProjectProfile> {
    // 1. Find a .csproj in the file list
    let csproj_name = files.iter().find(|f| f.ends_with(".csproj"))?;

    // 2. Read and check for Aspire AppHost signals
    let csproj_path = join_path(&root, csproj_name);
    let csproj = match read_host_file(&csproj_path) {
        Ok(c) => c,
        Err(e) => {
            log!("Aspire plugin: failed to read {}: {:?}", csproj_path, e);
            return None;
        }
    };

    if !is_aspire_apphost(&csproj) {
        return None;
    }

    log!("Aspire plugin: detected Aspire AppHost at {}", root);

    // 3. Build tags
    let mut tags = vec![".NET".to_string(), "C#".to_string(), "Aspire".to_string()];
    if let Some(tf) = detect_target_framework(&csproj) {
        tags.push(tf);
    }

    // 4. Parse resources from Program.cs / AppHost.cs (including OAS detection)
    let entry_points = detect_resource_entry_points(&root, &files);
    log!(
        "Aspire plugin: {} entry points detected",
        entry_points.len()
    );

    Some(ProjectProfile {
        category: "Aspire AppHost".to_string(),
        tags,
        entry_points,
    })
}

#[chorograph_plugin]
pub fn handle_action(_action_id: String, _payload: serde_json::Value) {
    // No-op for now
}

#[chorograph_plugin]
pub fn detect_run_status(root: String) -> Option<RunStatus> {
    // Quick check: confirm this is an Aspire AppHost
    let csproj = find_and_read_csproj(&root)?;
    if !is_aspire_apphost(&csproj) {
        return None;
    }

    // Probe the Aspire Dashboard default port (18888)
    let dashboard_port: u16 = 18888;
    let is_running = tcp_probe("localhost", dashboard_port);

    if !is_running {
        return Some(RunStatus {
            is_running: false,
            url: None,
            pid: None,
            resources: vec![],
        });
    }

    let dashboard_url = format!("http://localhost:{}", dashboard_port);
    Some(RunStatus {
        is_running: true,
        url: Some(dashboard_url),
        pid: None,
        resources: vec![],
    })
}

// ---------------------------------------------------------------------------
// Resource entry point detection
// ---------------------------------------------------------------------------

/// Parse builder.Add*() calls from Program.cs / AppHost.cs and return each
/// orchestrated resource as an EntryPoint. For project resources, also probes
/// the project source for OpenAPI registration and emits additional OAS entry points.
fn detect_resource_entry_points(root: &str, files: &[String]) -> Vec<EntryPoint> {
    // Find Program.cs or AppHost.cs (case-insensitive)
    let program_file = files.iter().find(|f| {
        let bare = f.rsplit('/').next().unwrap_or(f).to_lowercase();
        bare == "program.cs" || bare == "apphost.cs"
    });

    let rel_path = match program_file {
        Some(p) => p,
        None => return vec![],
    };

    let full_path = join_path(root, rel_path);
    let src = match read_host_file(&full_path) {
        Ok(s) => s,
        Err(e) => {
            log!("Aspire plugin: failed to read {}: {:?}", full_path, e);
            return vec![];
        }
    };

    let mut entry_points = Vec::new();

    for caps in RE_ADD_RESOURCE.captures_iter(&src) {
        let add_suffix = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        // Group 2 is the generic type name (only present for AddProject<T>)
        let type_name = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let resource_name = caps.get(3).map(|m| m.as_str()).unwrap_or("");

        // Compute 1-based line number from match offset
        let match_start = caps.get(0).map(|m| m.start()).unwrap_or(0);
        let line = (src[..match_start].chars().filter(|&c| c == '\n').count() + 1) as u32;

        let (label, kind) = classify_resource(add_suffix, resource_name);

        entry_points.push(EntryPoint {
            label,
            path: rel_path.clone(),
            line: Some(line),
            method: Some(kind.clone()),
            description: Some(format!("builder.Add{}(\"{}\")", add_suffix, resource_name)),
            detection_source: Some("regex".to_string()),
        });

        // For project resources, try to detect OAS in the referenced project's source
        if kind == "PROJECT" && !type_name.is_empty() {
            let oas_entries = detect_oas_entry_points(root, type_name, resource_name);
            entry_points.extend(oas_entries);
        }
    }

    entry_points
}

/// Given an Aspire-orchestrated project's generic type name (e.g. "MyApp_Api")
/// and its Aspire resource name (e.g. "api"), try to find the project directory,
/// read its Program.cs, and detect OpenAPI registration.
/// Returns a list of OAS entry points (e.g. "/openapi/v1.json").
fn detect_oas_entry_points(
    apphost_root: &str,
    type_name: &str,
    resource_name: &str,
) -> Vec<EntryPoint> {
    // The generic type name in Aspire uses underscores where the folder/namespace uses dots.
    // e.g. "MyApp_Api" → folder "MyApp.Api"
    // We'll try a few candidate folder names relative to common parent layouts:
    //   - sibling of the AppHost (most common: all projects under a shared src/ or repo root)
    //   - direct sibling of AppHost root
    let folder_name = type_name.replace('_', ".");

    // Determine the parent directory (one level up from AppHost root)
    let parent = apphost_root
        .trim_end_matches('/')
        .rsplit_once('/')
        .map(|(p, _)| p)
        .unwrap_or(apphost_root);

    // Candidate directories to probe:
    // 1. <parent>/<FolderName>          e.g. /repo/src/MyApp.Api
    // 2. <parent>/src/<FolderName>      (grandparent/src/<name> when AppHost is nested deeper)
    // 3. <apphost_root>/../<FolderName> (same as #1 via parent)
    let grandparent = parent.rsplit_once('/').map(|(p, _)| p).unwrap_or(parent);

    let candidates = [
        format!("{}/{}", parent, folder_name),
        format!("{}/src/{}", grandparent, folder_name),
        format!("{}/{}", grandparent, folder_name),
    ];

    for candidate_dir in &candidates {
        if let Some(entries) = try_read_oas_from_project(candidate_dir, resource_name) {
            return entries;
        }
    }

    vec![]
}

/// Try to read Program.cs (or App.cs / Startup.cs) from a candidate project
/// directory and detect OpenAPI registration. Returns None if the directory
/// doesn't look like a valid project.
fn try_read_oas_from_project(project_dir: &str, resource_name: &str) -> Option<Vec<EntryPoint>> {
    // Try to read a .csproj to confirm this is actually a project directory.
    // We derive the csproj name from the last path component.
    let basename = project_dir
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("");
    if basename.is_empty() {
        return None;
    }

    let csproj_path = format!("{}/{}.csproj", project_dir, basename);
    // If the csproj doesn't exist, this isn't the right directory
    if read_host_file(&csproj_path).is_err() {
        return None;
    }

    // Read Program.cs (the most common entry point file)
    let program_path = format!("{}/Program.cs", project_dir);
    let src = match read_host_file(&program_path) {
        Ok(s) => s,
        Err(_) => return None,
    };

    let oas_entries = extract_oas_entry_points_from_source(&src, resource_name, "Program.cs");
    Some(oas_entries)
}

/// Scan a source file for OpenAPI registrations and return entry points.
fn extract_oas_entry_points_from_source(
    src: &str,
    resource_name: &str,
    rel_path: &str,
) -> Vec<EntryPoint> {
    let mut entries = Vec::new();

    // Check for Swashbuckle first (less common in modern .NET)
    if RE_ADD_SWAGGER.is_match(src) {
        entries.push(EntryPoint {
            label: format!("{}: /swagger/v1/swagger.json", resource_name),
            path: rel_path.to_string(),
            line: None,
            method: Some("OAS".to_string()),
            description: Some("OpenAPI spec (Swashbuckle)".to_string()),
            detection_source: Some("regex".to_string()),
        });
        return entries;
    }

    // Collect document names from AddOpenApi calls
    let mut doc_names: Vec<String> = RE_ADD_OPENAPI
        .captures_iter(src)
        .map(|caps| {
            caps.get(1)
                .map(|m| m.as_str().to_string())
                // Empty string capture or no capture → default name "v1"
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "v1".to_string())
        })
        .collect();

    if doc_names.is_empty() {
        // No AddOpenApi found — no OAS in this project
        return entries;
    }

    // Deduplicate
    doc_names.sort();
    doc_names.dedup();

    // Check if MapOpenApi is also present (confirms the route is actually registered)
    let map_openapi_present = RE_MAP_OPENAPI.is_match(src);

    // If MapOpenApi is present, collect any custom path overrides
    // (e.g. MapOpenApi("/openapi/{documentName}/openapi.json"))
    let custom_paths: Vec<String> = if map_openapi_present {
        RE_MAP_OPENAPI
            .captures_iter(src)
            .filter_map(|caps| {
                caps.get(1).and_then(|m| {
                    let s = m.as_str();
                    // Only treat it as a custom path template if it contains '{'
                    if s.contains('{') {
                        Some(s.to_string())
                    } else {
                        None
                    }
                })
            })
            .collect()
    } else {
        vec![]
    };

    for doc in &doc_names {
        // If there's a custom path template, use it; otherwise use the default
        let spec_path = if let Some(template) = custom_paths.first() {
            template.replace("{documentName}", doc)
        } else {
            format!("/openapi/{}.json", doc)
        };

        let desc = if map_openapi_present {
            format!("OpenAPI spec for document \"{}\"", doc)
        } else {
            format!(
                "OpenAPI spec for document \"{}\" (MapOpenApi not detected — may be dev-only)",
                doc
            )
        };

        entries.push(EntryPoint {
            label: format!("{}: {}", resource_name, spec_path),
            path: rel_path.to_string(),
            line: None,
            method: Some("OAS".to_string()),
            description: Some(desc),
            detection_source: Some("regex".to_string()),
        });
    }

    entries
}

/// Map the Add* suffix + resource name to a human-readable label and a method/kind string.
fn classify_resource(add_suffix: &str, name: &str) -> (String, String) {
    let lower = add_suffix.to_lowercase();
    match lower.as_str() {
        "project" => (format!("project: {}", name), "PROJECT".to_string()),
        "postgres" | "postgresql" => (
            format!("container: {} (postgres)", name),
            "CONTAINER".to_string(),
        ),
        "redis" => (
            format!("container: {} (redis)", name),
            "CONTAINER".to_string(),
        ),
        "rabbitmq" => (
            format!("container: {} (rabbitmq)", name),
            "CONTAINER".to_string(),
        ),
        "mongodb" => (
            format!("container: {} (mongodb)", name),
            "CONTAINER".to_string(),
        ),
        "mysql" => (
            format!("container: {} (mysql)", name),
            "CONTAINER".to_string(),
        ),
        "kafka" => (
            format!("container: {} (kafka)", name),
            "CONTAINER".to_string(),
        ),
        "nats" => (
            format!("container: {} (nats)", name),
            "CONTAINER".to_string(),
        ),
        "sqlserver" => (
            format!("container: {} (sqlserver)", name),
            "CONTAINER".to_string(),
        ),
        "valkey" => (
            format!("container: {} (valkey)", name),
            "CONTAINER".to_string(),
        ),
        "npmapp" | "nodejsapp" => (format!("resource: {} (npm)", name), "RESOURCE".to_string()),
        "executable" => (
            format!("resource: {} (executable)", name),
            "RESOURCE".to_string(),
        ),
        "container" => (
            format!("container: {} (custom)", name),
            "CONTAINER".to_string(),
        ),
        _ => (
            format!("resource: {} ({})", name, add_suffix.to_lowercase()),
            "RESOURCE".to_string(),
        ),
    }
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

fn join_path(root: &str, file: &str) -> String {
    if root.ends_with('/') {
        format!("{}{}", root, file)
    } else {
        format!("{}/{}", root, file)
    }
}

fn detect_target_framework(csproj: &str) -> Option<String> {
    for ver in &["net9.0", "net8.0", "net7.0", "net6.0"] {
        if csproj.contains(&format!("<TargetFramework>{}</TargetFramework>", ver)) {
            return Some(ver.to_string());
        }
    }
    let caps = RE_TARGET_FRAMEWORK.captures(csproj)?;
    caps.get(1).map(|m| m.as_str().to_string())
}

/// Try to find and read the .csproj directly in the project root.
/// Derives the filename from the last path component of root.
fn find_and_read_csproj(root: &str) -> Option<String> {
    let basename = root.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    if !basename.is_empty() {
        let candidate = join_path(root, &format!("{}.csproj", basename));
        if let Ok(content) = read_host_file(&candidate) {
            return Some(content);
        }
    }
    None
}
