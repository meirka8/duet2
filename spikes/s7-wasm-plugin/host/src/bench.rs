//! S-7 spike: measure host<->guest call overhead for a trivial WASM
//! component implementing the `content-plugin` interface.
//!
//! Loads guest-content/guest_content.component.wasm, calls value() in a
//! tight loop after discarding warmup calls, and reports median/p99
//! per-call wall-clock latency.

use std::path::PathBuf;
use std::time::Instant;

use wasmtime::component::{bindgen, Component, Linker};
use wasmtime::{Config, Engine, Store};

bindgen!({
    world: "content-host",
    path: "../wit",
});

const WARMUP_CALLS: usize = 1_000;
const MEASURED_CALLS: usize = 200_000;

fn main() -> anyhow::Result<()> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)?;

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let wasm_path = manifest_dir.join("../guest-content/guest_content.component.wasm");
    let component = Component::from_file(&engine, &wasm_path)?;

    let linker = Linker::new(&engine);
    let mut store = Store::new(&engine, ());

    let instance = ContentHost::instantiate(&mut store, &component, &linker)?;
    let plugin = instance.duet_plugin_spike_content_plugin();

    let fields = plugin.call_fields(&mut store)?;
    println!("guest fields(): {fields:?}");

    // Warm up: let the JIT settle, page faults resolve, etc. Discarded.
    for i in 0..WARMUP_CALLS {
        let path = format!("/home/user/pictures/img_{i:05}.jpg");
        let _ = plugin.call_value(&mut store, &path, 0)?;
    }

    // Measured loop.
    let mut samples = Vec::with_capacity(MEASURED_CALLS);
    for i in 0..MEASURED_CALLS {
        let path = format!("/home/user/pictures/img_{i:05}.jpg");
        let start = Instant::now();
        let result = plugin.call_value(&mut store, &path, 0)?;
        let elapsed = start.elapsed();
        std::hint::black_box(&result);
        samples.push(elapsed.as_nanos() as u64);
    }

    samples.sort_unstable();
    let n = samples.len();
    let median = samples[n / 2];
    let p99 = samples[(n as f64 * 0.99) as usize];
    let p999 = samples[(n as f64 * 0.999) as usize];
    let min = samples[0];
    let max = samples[n - 1];
    let sum: u64 = samples.iter().sum();
    let mean = sum as f64 / n as f64;

    println!("\n=== per-call value() overhead over {n} calls (after {WARMUP_CALLS} warmup) ===");
    println!("min:    {min:>8} ns");
    println!("median: {median:>8} ns");
    println!("mean:   {mean:>8.1} ns");
    println!("p99:    {p99:>8} ns");
    println!("p99.9:  {p999:>8} ns");
    println!("max:    {max:>8} ns");

    Ok(())
}
