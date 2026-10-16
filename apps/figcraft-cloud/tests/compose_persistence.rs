//! S3-GATE: cloud roles keep no persistent state on their container's disk.
//!
//! Every service built from the cloud image (and the static web and gateway images) must mount no
//! volumes, so documents, snapshots, assets, libraries, plugins, exports and thumbnails can live
//! only in PostgreSQL, Redis or the S3-compatible object store. Only the three data services may
//! own volumes.

const COMPOSE: &str = include_str!("../../../deploy/saas/compose.yaml");
const DATA_SERVICES: [&str; 3] = ["postgres", "redis", "seaweedfs"];

/// `(service name, its block)` for every service under the top-level `services:` key.
fn services() -> Vec<(String, String)> {
    let mut services = Vec::new();
    let mut inside = false;
    let mut current: Option<(String, String)> = None;
    for line in COMPOSE.lines() {
        if !line.starts_with(' ') && !line.is_empty() {
            inside = line == "services:";
            if let Some(service) = current.take() {
                services.push(service);
            }
            continue;
        }
        if !inside {
            continue;
        }
        let is_service = line.starts_with("  ") && !line.starts_with("   ") && line.trim_end().ends_with(':') && !line.trim_start().starts_with('#');
        if is_service {
            if let Some(service) = current.take() {
                services.push(service);
            }
            current = Some((line.trim().trim_end_matches(':').to_string(), String::new()));
        } else if let Some((_, block)) = current.as_mut() {
            block.push_str(line);
            block.push('\n');
        }
    }
    services.extend(current);
    services
}

#[test]
fn only_data_services_own_volumes() {
    let services = services();
    let names: Vec<_> = services.iter().map(|(name, _)| name.as_str()).collect();
    for expected in ["cloud", "migrate", "seaweedfs-init", "web", "gateway", "postgres", "redis", "seaweedfs"] {
        assert!(names.contains(&expected), "compose.yaml should define {expected}: {names:?}");
    }
    for (name, block) in &services {
        let mounts = block.lines().any(|line| line.trim_start().starts_with("volumes:"));
        if DATA_SERVICES.contains(&name.as_str()) {
            assert!(mounts, "{name} should persist to a named volume");
        } else if name == "gateway" {
            // The gateway mounts only its read-only route configuration.
            let mount_lines: Vec<_> =
                block.lines().filter(|line| line.trim_start().starts_with("- ") && line.contains(':') && line.contains('/')).collect();
            assert!(mount_lines.iter().all(|line| line.trim_end().ends_with(":ro")), "gateway mounts must be read-only: {mount_lines:?}");
        } else {
            assert!(!mounts, "{name} must not mount volumes; cloud state belongs in PostgreSQL or object storage");
        }
    }
}
