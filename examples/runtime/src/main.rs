use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use futures::executor::block_on;
use wasi_frame_buffer_wasmtime::WasiFrameBufferView;
use wasi_graphics_context_wasmtime::WasiGraphicsContextView;
use wasi_mini_canvas_wasmtime::{MiniCanvas, MiniCanvasDesc, WasiMiniCanvasView};
use wasi_webgpu_wasmtime::WasiWebGpuView;
use wasmtime::{
    component::{Component, Linker},
    Config, Engine, Store,
};
use wasmtime_wasi::bindings::cli::{stderr, stdin, stdout};
use wasmtime_wasi::bindings::clocks::{monotonic_clock, wall_clock};
use wasmtime_wasi::bindings::io::error;
use wasmtime_wasi::bindings::random::random;
use wasmtime_wasi::bindings::sockets::{tcp, udp};

use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiView};

#[derive(clap::Parser, Debug)]
struct RuntimeArgs {
    /// The example name
    #[arg(long)]
    example: Option<String>,

    /// A Wasm component.
    #[arg(long)]
    wasm: Option<String>,
}

wasmtime::component::bindgen!({
    path: "../../wit/",
    world: "example",
    async: {
        only_imports: [],
    },
    with: {
        "wasi:webgpu/graphics-context": wasi_graphics_context_wasmtime,
        "wasi:webgpu/mini-canvas": wasi_mini_canvas_wasmtime,
        "wasi:webgpu/frame-buffer": wasi_frame_buffer_wasmtime,
        "wasi:webgpu/webgpu": wasi_webgpu_wasmtime,
    },
});

struct HostState {
    pub table: ResourceTable,
    pub ctx: WasiCtx,
    pub instance: Arc<wgpu_core::global::Global>,
    pub main_thread_proxy: wasi_mini_canvas_wasmtime::WasiWinitEventLoopProxy,
}

impl HostState {
    fn new(main_thread_proxy: wasi_mini_canvas_wasmtime::WasiWinitEventLoopProxy) -> Self {
        Self {
            table: ResourceTable::new(),
            ctx: WasiCtxBuilder::new().inherit_stdio().build(),
            instance: Arc::new(wgpu_core::global::Global::new(
                "webgpu",
                wgpu_types::InstanceDescriptor {
                    backends: wgpu_types::Backends::all(),
                    flags: wgpu_types::InstanceFlags::from_build_config(),
                    dx12_shader_compiler: wgpu_types::Dx12Compiler::Fxc,
                    gles_minor_version: wgpu_types::Gles3MinorVersion::default(),
                },
            )),
            main_thread_proxy,
        }
    }
}

impl WasiView for HostState {
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }

    fn ctx(&mut self) -> &mut WasiCtx {
        &mut self.ctx
    }
}

impl WasiGraphicsContextView for HostState {}
impl WasiFrameBufferView for HostState {}

struct UiThreadSpawner(wasi_mini_canvas_wasmtime::WasiWinitEventLoopProxy);

impl wasi_webgpu_wasmtime::MainThreadSpawner for UiThreadSpawner {
    async fn spawn<F, T>(&self, f: F) -> T
    where
        F: FnOnce() -> T + Send + Sync + 'static,
        T: Send + Sync + 'static,
    {
        self.0.spawn(f).await
    }
}

impl WasiWebGpuView for HostState {
    fn instance(&self) -> Arc<wgpu_core::global::Global> {
        Arc::clone(&self.instance)
    }

    fn ui_thread_spawner(&self) -> Box<impl wasi_webgpu_wasmtime::MainThreadSpawner + 'static> {
        Box::new(UiThreadSpawner(self.main_thread_proxy.clone()))
    }
}

impl WasiMiniCanvasView for HostState {
    fn create_canvas(&self, desc: MiniCanvasDesc) -> MiniCanvas {
        block_on(self.main_thread_proxy.create_window(desc))
    }
}

impl ExampleImports for HostState {
    fn print(&mut self, s: String) {
        println!("{s}");
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .init();

    let args = RuntimeArgs::parse();

    let mut config = Config::default();
    config.wasm_component_model(true);
    config.async_support(true);
    let engine = Engine::new(&config)?;
    let mut linker: Linker<HostState> = Linker::new(&engine);

    wasi_webgpu_wasmtime::add_to_linker(&mut linker)?;
    wasi_frame_buffer_wasmtime::add_to_linker(&mut linker)?;
    wasi_graphics_context_wasmtime::add_to_linker(&mut linker)?;
    wasi_mini_canvas_wasmtime::add_to_linker(&mut linker)?;

    fn type_annotate<F>(val: F) -> F
    where
        F: Fn(&mut HostState) -> &mut dyn ExampleImports,
    {
        val
    }
    let closure = type_annotate::<_>(|t| t);
    Example::add_to_linker_imports_get_host(&mut linker, closure)?;

    fn type_annotate_wasi<T, F>(val: F) -> F
    where
        F: Fn(&mut T) -> &mut T,
    {
        val
    }
    let wasi_closure = type_annotate_wasi::<HostState, _>(|t| t);
    stdin::add_to_linker_get_host(&mut linker, wasi_closure)?;
    stdout::add_to_linker_get_host(&mut linker, wasi_closure)?;
    stderr::add_to_linker_get_host(&mut linker, wasi_closure)?;
    error::add_to_linker_get_host(&mut linker, wasi_closure)?;
    monotonic_clock::add_to_linker_get_host(&mut linker, wasi_closure)?;
    wall_clock::add_to_linker_get_host(&mut linker, wasi_closure)?;
    tcp::add_to_linker_get_host(&mut linker, wasi_closure)?;
    udp::add_to_linker_get_host(&mut linker, wasi_closure)?;
    random::add_to_linker_get_host(&mut linker, wasi_closure)?;

    let (main_thread_loop, main_thread_proxy) =
        wasi_mini_canvas_wasmtime::create_wasi_winit_event_loop();
    let host_state = HostState::new(main_thread_proxy);

    let mut store = Store::new(&engine, host_state);

    let wasm_path = match args.example {
        Some(ex) => format!("./target/example-{}.wasm", ex),
        _ => args.wasm.unwrap(),
    };

    let component =
        Component::from_file(&engine, &wasm_path).context("Component file not found")?;

    let (instance, _) = Example::instantiate_async(&mut store, &component, &linker)
        .await
        .unwrap();

    tokio::spawn(async move {
        instance.call_start(&mut store).await.unwrap();
    });

    main_thread_loop.run();

    Ok(())
}
