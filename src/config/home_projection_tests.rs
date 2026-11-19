use super::Config;

const HOME_DEFAULT: &[u8] = include_bytes!("home_default_downstream.yaml");

#[test]
fn actual_home_default_fixture_has_no_unsupported_execution_controls() {
    let config = Config::parse(HOME_DEFAULT).unwrap();
    assert!(config.unsupported_features().is_empty());
    assert!(!String::from_utf8_lossy(HOME_DEFAULT).contains("ws-auth:"));
    assert_eq!(config.lifecycle.lifecycle_config_revision, 7);
}

/// Runs real Home Config marshaling and its private downstream sanitizer without
/// adding or changing any Go source in Home. Go's overlay supplies one test file.
/// Requires the sibling checkout and its Go toolchain/dependencies.
#[test]
#[ignore = "requires sibling AI48Home and Go; run explicitly with --ignored --nocapture"]
fn live_home_config_projection() {
    use std::{collections::BTreeMap, path::Path, process::Command};
    let home = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../AI48Home")
        .canonicalize()
        .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("projection.go");
    let output = temp.path().join("downstream.yaml");
    std::fs::write(&source, r#"package home
import (
    "os"
    "testing"
    appconfig "github.com/router-for-me/CLIProxyAPIHome/internal/config"
    "gopkg.in/yaml.v3"
)
func TestRustRuntimeProjection(t *testing.T) {
    cfg := appconfig.Config{
        Port: appconfig.DefaultCPAPort,
        CredentialConcurrency: appconfig.DefaultCredentialConcurrencyConfig(),
        CredentialInFlight: appconfig.DefaultCredentialInFlightConfig(),
    }
    // Active Home assigns this database-backed revision before distribution.
    cfg.CredentialConcurrency.LifecycleConfigRevision = 7
    appconfig.ForceHomeRuntimeConfig(&cfg)
    source, err := yaml.Marshal(cfg)
    if err != nil { t.Fatal(err) }
    downstream, err := sanitizeConfigYAMLForDownstream(source)
    if err != nil { t.Fatal(err) }
    if err := os.WriteFile(os.Getenv("AI48_RUST_PROJECTION_OUTPUT"), downstream, 0600); err != nil { t.Fatal(err) }
    // Simulate older stored Home configs carrying both auth spellings.
    var root map[string]any
    if err := yaml.Unmarshal(source, &root); err != nil { t.Fatal(err) }
    root["ws-auth"] = true
    root["oauth"] = map[string]any{"providers": map[string]any{"aistudio": map[string]any{"ws-auth": true}}}
    source, err = yaml.Marshal(root)
    if err != nil { t.Fatal(err) }
    downstream, err = sanitizeConfigYAMLForDownstream(source)
    if err != nil { t.Fatal(err) }
    if err := os.WriteFile(os.Getenv("AI48_RUST_PROJECTION_OUTPUT")+".ws.yaml", downstream, 0600); err != nil { t.Fatal(err) }
}
"#).unwrap();
    let mut replacements = BTreeMap::new();
    replacements.insert(
        home.join("internal/home/rust_runtime_projection_test.go"),
        source,
    );
    let overlay = temp.path().join("overlay.json");
    std::fs::write(
        &overlay,
        serde_json::to_vec(&serde_json::json!({"Replace": replacements})).unwrap(),
    )
    .unwrap();
    let status = Command::new("go")
        .current_dir(home)
        .arg("test")
        .arg(format!("-overlay={}", overlay.display()))
        .args([
            "-run",
            "^TestRustRuntimeProjection$",
            "-count=1",
            "./internal/home",
        ])
        .env("AI48_RUST_PROJECTION_OUTPUT", &output)
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "Home projection failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let legacy_auth = std::fs::read(format!("{}.ws.yaml", output.display())).unwrap();
    assert!(!String::from_utf8_lossy(&legacy_auth).contains("ws-auth:"));
    assert!(
        Config::parse(&legacy_auth)
            .unwrap()
            .unsupported_features()
            .is_empty()
    );
    let bytes = std::fs::read(output).unwrap();
    println!(
        "Actual sanitized Home Config:\n{}",
        String::from_utf8_lossy(&bytes)
    );
    assert_eq!(
        serde_yaml::from_slice::<serde_yaml::Value>(&bytes).unwrap(),
        serde_yaml::from_slice::<serde_yaml::Value>(HOME_DEFAULT).unwrap(),
        "Home's projection changed; inspect it and refresh the fixture"
    );
    let config = Config::parse(&bytes).unwrap();
    assert!(
        config.unsupported_features().is_empty(),
        "{:?}",
        config.unsupported_features()
    );
    assert!(!String::from_utf8_lossy(&bytes).contains("ws-auth:"));
    assert_eq!(config.port, 8317);
    assert_eq!(config.lifecycle.lifecycle_config_revision, 7);
}
