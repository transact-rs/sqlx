use std::env;
use std::path::PathBuf;
use std::process::Command;

fn build_wasm_component(component_name: &str) -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let component_dir = manifest_dir
        .join("tests/postgres/wasm-components")
        .join(component_name);

    println!("Building component: {}", component_name);

    let output = Command::new("cargo")
        .current_dir(&component_dir)
        .args([
            "+nightly",
            "build",
            "--target",
            "wasm32-wasip2",
            "--release",
        ])
        .output()
        .expect("Failed to build WASM component");

    if !output.status.success() {
        panic!(
            "Failed to build {}: {}",
            component_name,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    manifest_dir
        .join("target/wasm32-wasip2/release")
        .join(format!("{}.wasm", component_name.replace('-', "_")))
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

    let database_url = env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:password@127.0.0.1:5432/sqlx".to_string());

    let mut cmd = Command::new("wasmtime");
    cmd.args([
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

    println!("{} passed!", test_name);
    Ok(())
}

#[test]
fn test_wasi_postgres_connect() {
    let wasm = build_wasm_component("postgres-connect-test");
    run_wasm_test(wasm, "Postgres Connect Test").expect("Postgres connect test failed");
}

#[test]
fn test_wasi_postgres_execute_query() {
    let wasm = build_wasm_component("postgres-execute-query-test");
    run_wasm_test(wasm, "Postgres Execute Query Test").expect("Postgres execute query test failed");
}

#[test]
fn test_wasi_postgres_prepared_query() {
    let wasm = build_wasm_component("postgres-prepared-query-test");
    run_wasm_test(wasm, "Postgres Prepared Query Test")
        .expect("Postgres prepared query test failed");
}

#[test]
fn test_wasi_postgres_pool_crud() {
    let wasm = build_wasm_component("postgres-pool-crud-test");
    run_wasm_test(wasm, "Postgres Pool CRUD Test").expect("Postgres pool CRUD test failed");
}

#[test]
fn test_wasi_postgres_tls_connect() {
    if env::var("POSTGRES_WASI_RUN_TLS").as_deref() != Ok("1") {
        eprintln!("Skipping Postgres TLS Connect Test; set POSTGRES_WASI_RUN_TLS=1 to run it.");
        return;
    }

    let wasm = build_wasm_component("postgres-tls-connect-test");
    run_wasm_test_with_flags(wasm, "Postgres TLS Connect Test", &["-Stls=y"])
        .expect("Postgres TLS connect test failed");
}
