//! S-7 spike: demonstrate that epoch-based interruption actually stops a
//! runaway `loop {}` guest component within the configured deadline
//! (design.md §9.9: "2 s per call for content plugins", FR-PLUG-06).
//!
//! A background thread ticks the engine's epoch every 100ms; the store is
//! given a 20-tick (~2s) deadline before calling into guest-loop's
//! value(), which never returns on its own. If interruption works, the
//! host regains control with a trap instead of hanging.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use wasmtime::component::{bindgen, Component, Linker};
use wasmtime::{Config, Engine, Store};

bindgen!({
    world: "content-host",
    path: "../wit",
});

const TICK: Duration = Duration::from_millis(100);
const DEADLINE_TICKS: u64 = 20; // 20 * 100ms ~= 2s, matching design.md's default

fn main() -> anyhow::Result<()> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.epoch_interruption(true);
    let engine = Engine::new(&config)?;

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let wasm_path = manifest_dir.join("../guest-loop/guest_loop.component.wasm");
    let component = Component::from_file(&engine, &wasm_path)?;

    let linker = Linker::new(&engine);
    let mut store = Store::new(&engine, ());
    store.set_epoch_deadline(DEADLINE_TICKS);

    // Background ticker thread stands in for wherever the real host would
    // drive its epoch clock (a timer on the I/O runtime, per §9.9). We
    // stop it once the call below returns so the process can exit cleanly.
    let stop = Arc::new(AtomicBool::new(false));
    let ticker_engine = engine.clone();
    let ticker_stop = stop.clone();
    let ticker = std::thread::spawn(move || {
        while !ticker_stop.load(Ordering::Relaxed) {
            std::thread::sleep(TICK);
            ticker_engine.increment_epoch();
        }
    });

    let instance = ContentHost::instantiate(&mut store, &component, &linker)?;
    let plugin = instance.duet_plugin_spike_content_plugin();

    println!(
        "calling guest-loop's value() (a `loop {{}}` that never returns); \
         epoch deadline = {DEADLINE_TICKS} ticks @ {TICK:?}/tick ~= {:?}",
        TICK * DEADLINE_TICKS as u32
    );
    let start = Instant::now();
    let result = plugin.call_value(&mut store, "/does/not/matter", 0);
    let elapsed = start.elapsed();

    stop.store(true, Ordering::Relaxed);
    ticker.join().expect("ticker thread panicked");

    match result {
        Ok(v) => {
            println!("UNEXPECTED: guest call returned normally: {v:?} after {elapsed:?}");
            std::process::exit(1);
        }
        Err(e) => {
            println!("guest call was interrupted after {elapsed:?}");
            println!("trap: {e}");
            println!(
                "host process is still alive and able to continue (this line \
                 printed after catching the trap, not after a crash/hang)."
            );
        }
    }

    Ok(())
}
