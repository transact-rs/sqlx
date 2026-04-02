use std::env;
use std::path::PathBuf;
use std::process::Command;

fn build_wasm_component(component_name: &str) -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let component_dir = manifest_dir
        .join("tests/mysql/wasm-components")
        .join(component_name);

    println!("Building component: {}", component_name);

    let output = Command::new("cargo")
        .current_dir(&component_dir)
        .args(&["build", "--target", "wasm32-wasip2", "--release"])
        .output()
        .expect("Failed to build WASM component");

    if !output.status.success() {
        panic!(
            "Failed to build {}: {}",
            component_name,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // WASM binaries are stored in the workspace root target directory
    manifest_dir
        .join("target/wasm32-wasip2/release")
        .join(format!("{}.wasm", component_name.replace("-", "_")))
}

fn run_wasm_test(wasm_path: PathBuf, test_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    run_wasm_test_with_flags(wasm_path, test_name, &[])
}

fn run_wasm_test_with_flags(
    wasm_path: PathBuf,
    test_name: &str,
    extra_flags: &[&str],
) -> Result<(), Box<dyn std::error::Error>> {
    println!("Running test: {}", test_name);

    let database_url = "mysql://mysql:Password123!@127.0.0.1:3306/todos";

    let mut cmd = Command::new("wasmtime");
    cmd.args(&[
        "run",
        "-Scli=y",
        "-Stcp=y",
        "-Sinherit-env=y",
        "-Sudp=y",
        "-Sp3",
        "-Sallow-ip-name-lookup=y",
        "-Wcomponent-model-async=y",
        "-Sinherit-network=y",
    ]);
    cmd.args(extra_flags);
    cmd.env("DATABASE_URL", database_url)
        .arg(wasm_path.as_os_str());

    let status = cmd.status()?;

    if !status.success() {
        return Err(format!("{} failed", test_name).into());
    }

    println!("✓ {} passed!", test_name);
    Ok(())
}

#[test]
fn test_wasi_mysql_connect() {
    let wasm = build_wasm_component("connect-test");
    run_wasm_test(wasm, "Connect Test").expect("Connect test failed");
}

#[test]
fn test_wasi_mysql_execute_query() {
    let wasm = build_wasm_component("execute-query-test");
    run_wasm_test(wasm, "Execute Query Test").expect("Execute query test failed");
}

#[test]
fn test_wasi_mysql_prepared_query() {
    let wasm = build_wasm_component("prepared-query-test");
    run_wasm_test(wasm, "Prepared Query Test").expect("Prepared query test failed");
}

#[test]
fn test_wasi_mysql_pool_crud() {
    let wasm = build_wasm_component("pool-crud-test");
    run_wasm_test(wasm, "Pool CRUD Test").expect("Pool CRUD test failed");
}

#[test]
fn test_wasi_mysql_tls_connect() {
    let wasm = build_wasm_component("tls-connect-test");
    run_wasm_test_with_flags(wasm, "TLS Connect Test", &["-Stls=y"])
        .expect("TLS connect test failed");
}
