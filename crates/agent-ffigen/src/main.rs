//! Generates Swift and Kotlin UniFFI bindings from the compiled `agent-sdk`
//! cdylib. Run with `cargo run -p agent-ffigen` (after `cargo build -p agent-sdk`).
//!
//! Uses the proc-macro ("library mode") codegen path of `uniffi_bindgen`
//! 0.28.3, matching the runtime version used by `agent-sdk`.

use camino::Utf8Path;
use uniffi_bindgen::bindings::{KotlinBindingGenerator, SwiftBindingGenerator};
use uniffi_bindgen::cargo_metadata::CrateConfigSupplier;
use uniffi_bindgen::library_mode;

fn main() -> anyhow::Result<()> {
    let lib = Utf8Path::new("target/debug/libagent_sdk.so");
    let supplier = CrateConfigSupplier::default();

    let swift_out = Utf8Path::new("bindings/swift/Sources/AgentSDK");
    let kotlin_out = Utf8Path::new("bindings/kotlin/agent-sdk/src/main/kotlin/com/agent");

    library_mode::generate_bindings(
        lib,
        None,
        &SwiftBindingGenerator,
        &supplier,
        None,
        swift_out,
        false,
    )?;
    println!("generated Swift bindings -> {swift_out}");

    library_mode::generate_bindings(
        lib,
        None,
        &KotlinBindingGenerator,
        &supplier,
        None,
        kotlin_out,
        false,
    )?;
    println!("generated Kotlin bindings -> {kotlin_out}");

    Ok(())
}
