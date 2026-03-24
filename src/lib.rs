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
///   2 — the resource name string argument (e.g. "api", "db", "cache")
static RE_ADD_RESOURCE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"builder\s*\.\s*Add([A-Za-z][A-Za-z0-9]*)\s*(?:<[^>]*>)?\s*\(\s*"([^"]+)""#)
        .unwrap()
});

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
    let csproj_files: Vec<&String> = files.iter().filter(|f| f.ends_with(".csproj")).collect();
    log!(
        "Aspire plugin: identify_project root={} files={} csproj_candidates={:?}",
        root,
        files.len(),
        csproj_files
    );

    let csproj_name = csproj_files.into_iter().next()?;

    // 2. Read and check for Aspire AppHost signals
    let csproj_path = join_path(&root, csproj_name);
    let csproj = match read_host_file(&csproj_path) {
        Ok(c) => c,
        Err(e) => {
            log!("Aspire plugin: failed to read {}: {:?}", csproj_path, e);
            return None;
        }
    };

    log!(
        "Aspire plugin: csproj preview (first 300 chars): {}",
        csproj.chars().take(300).collect::<String>()
    );

    if !is_aspire_apphost(&csproj) {
        log!("Aspire plugin: not an AppHost csproj, skipping");
        return None;
    }

    log!("Aspire plugin: detected Aspire AppHost at {}", root);

    // 3. Build tags
    let mut tags = vec![".NET".to_string(), "C#".to_string(), "Aspire".to_string()];
    if let Some(tf) = detect_target_framework(&csproj) {
        tags.push(tf);
    }

    // 4. Parse resources from Program.cs / AppHost.cs
    let entry_points = detect_resource_entry_points(&root, &files);
    log!(
        "Aspire plugin: detected {} entry points",
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
    // Quick check: find and read .csproj to confirm this is an Aspire AppHost
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

    // Dashboard is up — try to fetch per-resource status from the Dashboard API
    let dashboard_url = format!("http://localhost:{}", dashboard_port);
    let resources = fetch_resource_statuses(&dashboard_url);

    Some(RunStatus {
        is_running: true,
        url: Some(dashboard_url),
        pid: None,
        resources,
    })
}

// ---------------------------------------------------------------------------
// Resource entry point detection
// ---------------------------------------------------------------------------

/// Parse builder.Add*() calls from Program.cs / AppHost.cs and return each
/// orchestrated resource as an EntryPoint.
fn detect_resource_entry_points(root: &str, files: &[String]) -> Vec<EntryPoint> {
    // Find Program.cs or AppHost.cs (case-insensitive)
    let program_file = files.iter().find(|f| {
        let bare = f.rsplit('/').next().unwrap_or(f).to_lowercase();
        bare == "program.cs" || bare == "apphost.cs"
    });

    log!(
        "Aspire plugin: looking for Program.cs in {} files, found: {:?}",
        files.len(),
        program_file
    );

    let rel_path = match program_file {
        Some(p) => p,
        None => {
            log!(
                "Aspire plugin: no Program.cs found. Files list: {:?}",
                files
            );
            return vec![];
        }
    };

    let full_path = join_path(root, rel_path);
    let src = match read_host_file(&full_path) {
        Ok(s) => s,
        Err(e) => {
            log!("Aspire plugin: failed to read {}: {:?}", full_path, e);
            return vec![];
        }
    };

    log!(
        "Aspire plugin: Program.cs preview (first 400 chars): {}",
        src.chars().take(400).collect::<String>()
    );

    let mut entry_points = Vec::new();

    for caps in RE_ADD_RESOURCE.captures_iter(&src) {
        let add_suffix = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let resource_name = caps.get(2).map(|m| m.as_str()).unwrap_or("");

        log!(
            "Aspire plugin: found resource Add{}(\"{}\") ",
            add_suffix,
            resource_name
        );

        // Compute 1-based line number from match offset
        let match_start = caps.get(0).map(|m| m.start()).unwrap_or(0);
        let line = (src[..match_start].chars().filter(|&c| c == '\n').count() + 1) as u32;

        let (label, kind) = classify_resource(add_suffix, resource_name);

        entry_points.push(EntryPoint {
            label,
            path: rel_path.clone(),
            line: Some(line),
            method: Some(kind),
            description: Some(format!("builder.Add{}(\"{}\")", add_suffix, resource_name)),
            detection_source: Some("regex".to_string()),
        });
    }

    log!(
        "Aspire plugin: total entry points found: {}",
        entry_points.len()
    );
    entry_points
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
// Aspire Dashboard API — per-resource status
// ---------------------------------------------------------------------------

/// Attempt to fetch resource statuses from the Aspire Dashboard REST API.
/// Returns an empty vec if the API is unavailable or returns unexpected data.
/// The Aspire Dashboard exposes resources at: GET /api/v1/resources
fn fetch_resource_statuses(dashboard_url: &str) -> Vec<ResourceStatus> {
    let url = format!("{}/api/v1/resources", dashboard_url);

    let response = match http_get(&url, None) {
        Ok(r) => r,
        Err(e) => {
            log!(
                "Aspire plugin: Dashboard API unavailable at {}: {:?}",
                url,
                e
            );
            return vec![];
        }
    };

    if response.status != 200 {
        log!(
            "Aspire plugin: Dashboard API returned status {}",
            response.status
        );
        return vec![];
    }

    parse_dashboard_resources(&response.body)
}

/// Parse the Aspire Dashboard /api/v1/resources JSON response into ResourceStatus vec.
///
/// The Dashboard returns a structure like:
/// {
///   "resources": [
///     {
///       "name": "api",
///       "resourceType": "Project",
///       "state": "Running",
///       "urls": [{ "fullUrl": "https://localhost:7241", ... }],
///       ...
///     },
///     ...
///   ]
/// }
fn parse_dashboard_resources(body: &str) -> Vec<ResourceStatus> {
    // Parse as generic JSON — avoid a heavyweight schema dependency
    let root: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => {
            log!("Aspire plugin: failed to parse Dashboard response: {}", e);
            return vec![];
        }
    };

    let resources_arr = match root.get("resources").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => {
            log!("Aspire plugin: no 'resources' array in Dashboard response");
            return vec![];
        }
    };

    let mut out = Vec::new();

    for item in resources_arr {
        let name = item
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if name.is_empty() {
            continue;
        }

        let resource_type = item
            .get("resourceType")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_lowercase();

        let state = item
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        // Extract the first URL from the urls array
        let url = item
            .get("urls")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|u| u.get("fullUrl"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        out.push(ResourceStatus {
            name,
            kind: resource_type,
            state,
            url,
        });
    }

    out
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
