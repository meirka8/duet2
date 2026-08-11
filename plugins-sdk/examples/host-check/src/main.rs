//! T-2.6.1 validation harness -- see Cargo.toml's package comment.
//!
//! 1. `wasmtime::component::bindgen!` for all five plugin-class worlds:
//!    proves the host-side Rust bindings generate and type-check for
//!    every interface in plugins-sdk/wit/, including the `host` interface's
//!    `granted-stream` resource (a host-defined resource -- the part S-7's
//!    spike didn't exercise, since its guest had zero host imports).
//! 2. `main()` actually instantiates plugins-sdk/examples/stub-content's
//!    built component and calls `fields()`/`value()` on it, implementing
//!    the generated `Host`/`HostGrantedStream` traits for real -- an
//!    end-to-end guest<->host round trip through the new WIT, not just a
//!    macro-expansion smoke test.

use wasmtime::component::{Component, HasSelf, Linker, Resource, ResourceTable};
use wasmtime::{Config, Engine, Store};

mod content {
    wasmtime::component::bindgen!({
        world: "content-plugin-world",
        path: "../../wit",
        // Map the WIT-defined `granted-stream` resource directly onto our
        // own `FakeStream` type instead of an opaque bindgen-generated
        // placeholder, so the Host impl below can store real data in it.
        with: { "duet:plugin/host.granted-stream": super::FakeStream },
    });
}
mod packer {
    wasmtime::component::bindgen!({
        world: "packer-plugin-world",
        path: "../../wit",
    });
}
mod fs {
    wasmtime::component::bindgen!({
        world: "fs-plugin-world",
        path: "../../wit",
    });
}
mod viewer {
    wasmtime::component::bindgen!({
        world: "viewer-plugin-world",
        path: "../../wit",
    });
}
mod command {
    wasmtime::component::bindgen!({
        world: "command-plugin-world",
        path: "../../wit",
    });
}

// Re-import under the names used by the live round-trip below.
use content::duet::plugin::host::{Error, Handle, Level};
use content::ContentPluginWorld;

/// A trivial in-memory "granted stream": the whole point of the handle
/// design is that the host, not the guest, decides what a handle refers
/// to. Here it always resolves to the same 4 bytes, which is enough to
/// exercise open-granted -> resource -> read() through the real component
/// boundary.
pub struct FakeStream {
    data: Vec<u8>,
    pos: usize,
}

#[derive(Default)]
struct HostState {
    table: ResourceTable,
}

impl content::duet::plugin::host::HostGrantedStream for HostState {
    fn read(&mut self, self_: Resource<FakeStream>, max_len: u32) -> Result<Vec<u8>, Error> {
        let stream = self.table.get_mut(&self_).map_err(|_| Error::Internal("bad handle".into()))?;
        let end = (stream.pos + max_len as usize).min(stream.data.len());
        let chunk = stream.data[stream.pos..end].to_vec();
        stream.pos = end;
        Ok(chunk)
    }

    fn write(&mut self, _self_: Resource<FakeStream>, bytes: Vec<u8>) -> Result<u32, Error> {
        Ok(bytes.len() as u32)
    }

    fn drop(&mut self, rep: Resource<FakeStream>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

// The `types` interface declares no functions of its own, but every
// interface a world touches gets a `Host` trait (empty here) that must be
// implemented for the world's `add_to_linker` bound to be satisfied.
impl content::duet::plugin::types::Host for HostState {}

impl content::duet::plugin::host::Host for HostState {
    fn open_granted(&mut self, h: Handle) -> Result<Resource<FakeStream>, Error> {
        // A real host would look `h` up in a per-call grant table built
        // from the manifest; here every handle resolves to the same fake
        // 4-byte payload, since this harness only proves the boundary
        // works, not capability enforcement (that's T-8.1.2).
        let _ = h;
        self.table
            .push(FakeStream {
                data: vec![0xDE, 0xAD, 0xBE, 0xEF],
                pos: 0,
            })
            .map_err(|_| Error::Internal("resource table full".into()))
    }

    fn progress(&mut self, _done: u64, _total: u64) -> bool {
        true
    }

    fn log(&mut self, level: Level, msg: String) {
        println!("[guest log {level:?}] {msg}");
    }

    fn secret_get(&mut self, _key: String) -> Result<String, Error> {
        Err(Error::PermissionDenied)
    }
}

fn main() -> anyhow::Result<()> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)?;

    let wasm_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../stub-content/stub_content.component.wasm");
    let component = Component::from_file(&engine, &wasm_path)?;

    let mut linker = Linker::new(&engine);
    ContentPluginWorld::add_to_linker::<_, HasSelf<_>>(&mut linker, |s| s)?;

    let mut store = Store::new(&engine, HostState::default());
    let instance = ContentPluginWorld::instantiate(&mut store, &component, &linker)?;
    let plugin = instance.duet_plugin_content_plugin();

    let fields = plugin.call_fields(&mut store)?;
    println!("guest fields(): {fields:?}");

    let entry = content::duet::plugin::types::Entry {
        name: "photo.jpg".to_string(),
        size: 4096,
        mtime: 0,
        mode: 0o644,
        is_dir: false,
        is_symlink: false,
    };

    // Field 0: metadata-only, no handle needed.
    let target0 = content::exports::duet::plugin::content_plugin::Target {
        meta: entry.clone(),
        content: None,
    };
    let v0 = plugin.call_value(&mut store, &target0, 0)?;
    println!("value(field 0, no handle): {v0:?}");
    assert!(matches!(
        v0,
        Ok(content::exports::duet::plugin::content_plugin::FieldValue::Integer(9))
    ));

    // Field 1: exercises open-granted() -> resource -> read() for real.
    let target1 = content::exports::duet::plugin::content_plugin::Target {
        meta: entry,
        content: Some(7), // any handle -- HostState::open_granted ignores it
    };
    let v1 = plugin.call_value(&mut store, &target1, 1)?;
    println!("value(field 1, with handle): {v1:?}");
    assert!(matches!(
        v1,
        Ok(content::exports::duet::plugin::content_plugin::FieldValue::Integer(4))
    ));

    println!("\nhost<->guest round trip through duet:plugin@0.1.0's content-plugin-world: OK");
    Ok(())
}
