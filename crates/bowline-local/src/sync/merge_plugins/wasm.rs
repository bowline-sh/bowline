use std::{error::Error, fmt};

const MERGE_EXPORT: &str = "bowline_merge";
const VALIDATE_EXPORT: &str = "bowline_validate";
const ALLOC_EXPORT: &str = "bowline_alloc";
const MEMORY_EXPORT: &str = "memory";
const WASM_FUEL: u64 = 5_000_000;
const WASM_MEMORY_MAX_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub(super) struct WasmPluginLimits {
    pub(super) max_output_bytes: usize,
    pub(super) fuel: u64,
    pub(super) max_memory_bytes: usize,
}

impl WasmPluginLimits {
    pub(super) fn default_with_output_limit(max_output_bytes: usize) -> Self {
        Self {
            max_output_bytes,
            fuel: WASM_FUEL,
            max_memory_bytes: WASM_MEMORY_MAX_BYTES,
        }
    }
}

pub(super) fn merge_with_wasm_plugin(
    engine: &wasmtime::Engine,
    module: &wasmtime::Module,
    path: &str,
    base: &[u8],
    local: &[u8],
    remote: &[u8],
    limits: WasmPluginLimits,
) -> Result<Option<Vec<u8>>, WasmMergeError> {
    let first = merge_with_wasm_plugin_once(engine, module, path, base, local, remote, limits)?;
    let second = merge_with_wasm_plugin_once(engine, module, path, base, local, remote, limits)?;
    require_deterministic_output(first, second)
}

pub(super) fn merge_plugin_engine() -> Result<wasmtime::Engine, WasmMergeError> {
    let mut config = wasmtime::Config::new();
    config.consume_fuel(true);
    config.wasm_multi_memory(false);
    Ok(wasmtime::Engine::new(&config)?)
}

pub(super) fn compile_merge_plugin_module(
    engine: &wasmtime::Engine,
    module_bytes: &[u8],
) -> Result<wasmtime::Module, WasmMergeError> {
    Ok(wasmtime::Module::new(engine, module_bytes)?)
}

fn require_deterministic_output(
    first: Option<Vec<u8>>,
    second: Option<Vec<u8>>,
) -> Result<Option<Vec<u8>>, WasmMergeError> {
    if first != second {
        return Err(WasmMergeError::NonDeterministicOutput);
    }
    Ok(first)
}

fn merge_with_wasm_plugin_once(
    engine: &wasmtime::Engine,
    module: &wasmtime::Module,
    path: &str,
    base: &[u8],
    local: &[u8],
    remote: &[u8],
    limits: WasmPluginLimits,
) -> Result<Option<Vec<u8>>, WasmMergeError> {
    let mut store = wasmtime::Store::new(
        engine,
        WasmStoreState {
            limits: wasmtime::StoreLimitsBuilder::new()
                .memory_size(limits.max_memory_bytes)
                .memories(1)
                .tables(1)
                .instances(1)
                .trap_on_grow_failure(true)
                .build(),
        },
    );
    store.limiter(|state| &mut state.limits);
    set_fuel(&mut store, limits.fuel)?;
    let instance = wasmtime::Instance::new(&mut store, module, &[])?;
    let memory = instance
        .get_memory(&mut store, MEMORY_EXPORT)
        .ok_or(WasmMergeError::MissingExport(MEMORY_EXPORT))?;
    let merge = instance
        .get_typed_func::<(i32, i32, i32, i32, i32, i32, i32, i32), i64>(&mut store, MERGE_EXPORT)
        .map_err(|_| WasmMergeError::MissingExport(MERGE_EXPORT))?;
    let validate = instance
        .get_typed_func::<(i32, i32, i32, i32), i32>(&mut store, VALIDATE_EXPORT)
        .map_err(|_| WasmMergeError::MissingExport(VALIDATE_EXPORT))?;
    let alloc = instance
        .get_typed_func::<i32, i32>(&mut store, ALLOC_EXPORT)
        .map_err(|_| WasmMergeError::MissingExport(ALLOC_EXPORT))?;

    let input_len = input_byte_len(base, local, remote, path.as_bytes())?;
    let input_ptr = call_wasm(|| alloc.call(&mut store, input_len as i32))?;
    if input_ptr < 0 {
        return Err(WasmMergeError::AllocationFailed);
    }
    let layout = InputLayout::new(input_ptr as usize, base, local, remote, path.as_bytes())?;
    let memory_len = memory.data_size(&store);
    if layout.total_len > memory_len {
        return Err(WasmMergeError::InputOutOfBounds {
            ptr: input_ptr as usize,
            len: input_len,
            memory_len,
        });
    }
    memory.write(&mut store, layout.base_ptr, base)?;
    memory.write(&mut store, layout.local_ptr, local)?;
    memory.write(&mut store, layout.remote_ptr, remote)?;
    memory.write(&mut store, layout.path_ptr, path.as_bytes())?;

    let result = call_wasm(|| {
        merge.call(
            &mut store,
            (
                layout.base_ptr as i32,
                base.len() as i32,
                layout.local_ptr as i32,
                local.len() as i32,
                layout.remote_ptr as i32,
                remote.len() as i32,
                layout.path_ptr as i32,
                path.len() as i32,
            ),
        )
    })?;
    if result < 0 {
        return Ok(None);
    }
    let output_ptr = ((result as u64) >> 32) as usize;
    let output_len = (result as u64 & 0xffff_ffff) as usize;
    if output_len > limits.max_output_bytes {
        return Err(WasmMergeError::OutputTooLarge {
            len: output_len,
            max: limits.max_output_bytes,
        });
    }
    let memory_len = memory.data_size(&store);
    if output_ptr
        .checked_add(output_len)
        .is_none_or(|end| end > memory_len)
    {
        return Err(WasmMergeError::OutputOutOfBounds {
            ptr: output_ptr,
            len: output_len,
            memory_len,
        });
    }
    let output = memory.data(&store)[output_ptr..output_ptr + output_len].to_vec();
    let valid = call_wasm(|| {
        validate.call(
            &mut store,
            (
                output_ptr as i32,
                output_len as i32,
                layout.path_ptr as i32,
                path.len() as i32,
            ),
        )
    })?;
    if valid == 1 {
        let validated = &memory.data(&store)[output_ptr..output_ptr + output_len];
        if validated == output.as_slice() {
            Ok(Some(output))
        } else {
            Ok(None)
        }
    } else {
        Ok(None)
    }
}

fn set_fuel<T>(store: &mut wasmtime::Store<T>, fuel: u64) -> Result<(), WasmMergeError> {
    store.set_fuel(fuel).map_err(map_wasm_error)
}

fn call_wasm<T>(call: impl FnOnce() -> Result<T, wasmtime::Error>) -> Result<T, WasmMergeError> {
    call().map_err(map_wasm_error)
}

fn map_wasm_error(error: wasmtime::Error) -> WasmMergeError {
    if matches!(
        error.downcast_ref::<wasmtime::Trap>(),
        Some(wasmtime::Trap::OutOfFuel)
    ) {
        WasmMergeError::ComputeBudgetExhausted
    } else {
        WasmMergeError::Runtime(error)
    }
}

fn input_byte_len(
    base: &[u8],
    local: &[u8],
    remote: &[u8],
    path: &[u8],
) -> Result<usize, WasmMergeError> {
    let len = base
        .len()
        .checked_add(local.len())
        .and_then(|len| len.checked_add(remote.len()))
        .and_then(|len| len.checked_add(path.len()))
        .ok_or(WasmMergeError::InputTooLarge)?;
    if len > i32::MAX as usize {
        return Err(WasmMergeError::InputTooLarge);
    }
    Ok(len)
}

struct WasmStoreState {
    limits: wasmtime::StoreLimits,
}

struct InputLayout {
    base_ptr: usize,
    local_ptr: usize,
    remote_ptr: usize,
    path_ptr: usize,
    total_len: usize,
}

impl InputLayout {
    fn new(
        base_offset: usize,
        base: &[u8],
        local: &[u8],
        remote: &[u8],
        path: &[u8],
    ) -> Result<Self, WasmMergeError> {
        let base_ptr = base_offset;
        let local_ptr = base_ptr
            .checked_add(base.len())
            .ok_or(WasmMergeError::InputTooLarge)?;
        let remote_ptr = local_ptr
            .checked_add(local.len())
            .ok_or(WasmMergeError::InputTooLarge)?;
        let path_ptr = remote_ptr
            .checked_add(remote.len())
            .ok_or(WasmMergeError::InputTooLarge)?;
        let total_len = path_ptr
            .checked_add(path.len())
            .ok_or(WasmMergeError::InputTooLarge)?;
        if total_len > i32::MAX as usize {
            return Err(WasmMergeError::InputTooLarge);
        }
        Ok(Self {
            base_ptr,
            local_ptr,
            remote_ptr,
            path_ptr,
            total_len,
        })
    }
}

#[derive(Debug)]
pub(super) enum WasmMergeError {
    Runtime(wasmtime::Error),
    Memory(wasmtime::MemoryAccessError),
    ComputeBudgetExhausted,
    MissingExport(&'static str),
    AllocationFailed,
    InputTooLarge,
    InputOutOfBounds {
        ptr: usize,
        len: usize,
        memory_len: usize,
    },
    OutputTooLarge {
        len: usize,
        max: usize,
    },
    OutputOutOfBounds {
        ptr: usize,
        len: usize,
        memory_len: usize,
    },
    NonDeterministicOutput,
}

impl fmt::Display for WasmMergeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(error) => write!(formatter, "{error}"),
            Self::Memory(error) => write!(formatter, "{error}"),
            Self::ComputeBudgetExhausted => formatter.write_str("WASM compute budget exhausted"),
            Self::MissingExport(name) => write!(formatter, "missing WASM export `{name}`"),
            Self::AllocationFailed => write!(formatter, "WASM input allocation failed"),
            Self::InputTooLarge => write!(formatter, "WASM merge input is too large"),
            Self::InputOutOfBounds {
                ptr,
                len,
                memory_len,
            } => write!(
                formatter,
                "WASM input pointer {ptr} plus length {len} exceeds memory {memory_len}"
            ),
            Self::OutputTooLarge { len, max } => {
                write!(formatter, "WASM output is {len} bytes, max is {max}")
            }
            Self::OutputOutOfBounds {
                ptr,
                len,
                memory_len,
            } => write!(
                formatter,
                "WASM output pointer {ptr} plus length {len} exceeds memory {memory_len}"
            ),
            Self::NonDeterministicOutput => {
                formatter.write_str("WASM merge output is not deterministic")
            }
        }
    }
}

impl Error for WasmMergeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Runtime(error) => Some(error.as_ref()),
            Self::Memory(error) => Some(error),
            Self::ComputeBudgetExhausted
            | Self::MissingExport(_)
            | Self::AllocationFailed
            | Self::InputTooLarge
            | Self::InputOutOfBounds { .. }
            | Self::OutputTooLarge { .. }
            | Self::OutputOutOfBounds { .. }
            | Self::NonDeterministicOutput => None,
        }
    }
}

impl From<wasmtime::Error> for WasmMergeError {
    fn from(error: wasmtime::Error) -> Self {
        map_wasm_error(error)
    }
}

impl From<wasmtime::MemoryAccessError> for WasmMergeError {
    fn from(error: wasmtime::MemoryAccessError) -> Self {
        Self::Memory(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compiled_module(bytes: &[u8]) -> (wasmtime::Engine, wasmtime::Module) {
        let engine = merge_plugin_engine().expect("engine builds");
        let module = compile_merge_plugin_module(&engine, bytes).expect("module compiles");
        (engine, module)
    }

    #[test]
    fn wasm_plugin_merges_and_validates_candidate_bytes() {
        let module = wat::parse_str(
            r#"
(module
  (memory (export "memory") 1)
  (func (export "bowline_alloc") (param i32) (result i32)
    i32.const 2048)
  (func (export "bowline_merge")
    (param $base_ptr i32) (param $base_len i32)
    (param $local_ptr i32) (param $local_len i32)
    (param $remote_ptr i32) (param $remote_len i32)
    (param $path_ptr i32) (param $path_len i32)
    (result i64)
    i32.const 4096
    i32.const 77
    i32.store8
    i64.const 17592186044417)
  (func (export "bowline_validate")
    (param $ptr i32) (param $len i32) (param $path_ptr i32) (param $path_len i32)
    (result i32)
    local.get $len
    i32.const 1
    i32.eq))
"#,
        )
        .expect("wat parses");
        let (engine, module) = compiled_module(&module);

        let merged = merge_with_wasm_plugin(
            &engine,
            &module,
            "notebook.ipynb",
            b"base",
            b"local",
            b"remote",
            WasmPluginLimits::default_with_output_limit(1024),
        )
        .expect("plugin runs")
        .expect("plugin returns bytes");

        assert_eq!(merged, b"M");
    }

    #[test]
    fn wasm_plugin_inputs_do_not_clobber_low_memory_data() {
        let module = wat::parse_str(
            r#"
(module
  (memory (export "memory") 1)
  (data (i32.const 0) "S")
  (func (export "bowline_alloc") (param i32) (result i32)
    i32.const 2048)
  (func (export "bowline_merge")
    (param i32) (param i32) (param i32) (param i32)
    (param i32) (param i32) (param i32) (param i32)
    (result i64)
    i32.const 0
    i32.load8_u
    i32.const 83
    i32.ne
    if
      i64.const -1
      return
    end
    i32.const 4096
    i32.const 77
    i32.store8
    i64.const 17592186044417)
  (func (export "bowline_validate")
    (param i32) (param $len i32) (param i32) (param i32)
    (result i32)
    local.get $len
    i32.const 1
    i32.eq))
"#,
        )
        .expect("wat parses");
        let (engine, module) = compiled_module(&module);

        let merged = merge_with_wasm_plugin(
            &engine,
            &module,
            "notebook.ipynb",
            b"base",
            b"local",
            b"remote",
            WasmPluginLimits::default_with_output_limit(1024),
        )
        .expect("plugin runs")
        .expect("plugin sees intact data segment");

        assert_eq!(merged, b"M");
    }

    #[test]
    fn wasm_plugin_validation_must_not_mutate_candidate_bytes() {
        let module = wat::parse_str(
            r#"
(module
  (memory (export "memory") 1)
  (func (export "bowline_alloc") (param i32) (result i32)
    i32.const 2048)
  (func (export "bowline_merge")
    (param i32) (param i32) (param i32) (param i32)
    (param i32) (param i32) (param i32) (param i32)
    (result i64)
    i32.const 4096
    i32.const 66
    i32.store8
    i64.const 17592186044417)
  (func (export "bowline_validate")
    (param $ptr i32) (param i32) (param i32) (param i32)
    (result i32)
    local.get $ptr
    i32.const 77
    i32.store8
    i32.const 1))
"#,
        )
        .expect("wat parses");
        let (engine, module) = compiled_module(&module);

        let merged = merge_with_wasm_plugin(
            &engine,
            &module,
            "notebook.ipynb",
            b"base",
            b"local",
            b"remote",
            WasmPluginLimits::default_with_output_limit(1024),
        )
        .expect("plugin runs");

        assert!(merged.is_none());
    }

    #[test]
    fn wasm_plugin_memory_growth_is_capped() {
        let module = wat::parse_str(
            r#"
(module
  (memory (export "memory") 1)
  (func (export "bowline_alloc") (param i32) (result i32)
    i32.const 2048)
  (func (export "bowline_merge")
    (param i32) (param i32) (param i32) (param i32)
    (param i32) (param i32) (param i32) (param i32)
    (result i64)
    i32.const 2000
    memory.grow
    drop
    i64.const -1)
  (func (export "bowline_validate")
    (param i32) (param i32) (param i32) (param i32)
    (result i32)
    i32.const 1))
"#,
        )
        .expect("wat parses");
        let (engine, module) = compiled_module(&module);

        let error = merge_with_wasm_plugin(
            &engine,
            &module,
            "huge.bin",
            b"base",
            b"local",
            b"remote",
            WasmPluginLimits::default_with_output_limit(1024),
        )
        .expect_err("memory growth traps");

        assert!(matches!(error, WasmMergeError::Runtime(_)));
    }

    #[test]
    fn wasm_plugin_fuel_exhaustion_is_compute_budget_error() {
        let module = wat::parse_str(
            r#"
(module
  (memory (export "memory") 1)
  (func (export "bowline_alloc") (param i32) (result i32)
    i32.const 2048)
  (func (export "bowline_merge")
    (param i32) (param i32) (param i32) (param i32)
    (param i32) (param i32) (param i32) (param i32)
    (result i64)
    (loop br 0)
    i64.const -1)
  (func (export "bowline_validate")
    (param i32) (param i32) (param i32) (param i32)
    (result i32)
    i32.const 1))
"#,
        )
        .expect("wat parses");
        let (engine, module) = compiled_module(&module);

        let error = merge_with_wasm_plugin(
            &engine,
            &module,
            "data.bin",
            b"base",
            b"local",
            b"remote",
            WasmPluginLimits::default_with_output_limit(1024),
        )
        .expect_err("fuel exhaustion is rejected");

        assert!(matches!(error, WasmMergeError::ComputeBudgetExhausted));
    }

    #[test]
    fn wasm_plugin_missing_exports_fail_safely() {
        let module = wat::parse_str(
            r#"
(module
  (memory (export "memory") 1)
  (func (export "bowline_alloc") (param i32) (result i32)
    i32.const 2048)
  (func (export "bowline_validate")
    (param i32) (param i32) (param i32) (param i32)
    (result i32)
    i32.const 1))
"#,
        )
        .expect("wat parses");
        let (engine, module) = compiled_module(&module);

        let error = merge_with_wasm_plugin(
            &engine,
            &module,
            "data.bin",
            b"base",
            b"local",
            b"remote",
            WasmPluginLimits::default_with_output_limit(1024),
        )
        .expect_err("missing merge export is rejected");

        assert!(matches!(error, WasmMergeError::MissingExport(MERGE_EXPORT)));
    }

    #[test]
    fn wasm_plugin_validate_trap_fails_safely() {
        let module = wat::parse_str(
            r#"
(module
  (memory (export "memory") 1)
  (func (export "bowline_alloc") (param i32) (result i32)
    i32.const 2048)
  (func (export "bowline_merge")
    (param i32) (param i32) (param i32) (param i32)
    (param i32) (param i32) (param i32) (param i32)
    (result i64)
    i32.const 4096
    i32.const 77
    i32.store8
    i64.const 17592186044417)
  (func (export "bowline_validate")
    (param i32) (param i32) (param i32) (param i32)
    (result i32)
    unreachable))
"#,
        )
        .expect("wat parses");
        let (engine, module) = compiled_module(&module);

        let error = merge_with_wasm_plugin(
            &engine,
            &module,
            "data.bin",
            b"base",
            b"local",
            b"remote",
            WasmPluginLimits::default_with_output_limit(1024),
        )
        .expect_err("validate trap is rejected");

        assert!(matches!(error, WasmMergeError::Runtime(_)));
    }

    #[test]
    fn wasm_plugin_oversized_output_fails_safely() {
        let module = wat::parse_str(
            r#"
(module
  (memory (export "memory") 1)
  (func (export "bowline_alloc") (param i32) (result i32)
    i32.const 2048)
  (func (export "bowline_merge")
    (param i32) (param i32) (param i32) (param i32)
    (param i32) (param i32) (param i32) (param i32)
    (result i64)
    i64.const 17592186044420)
  (func (export "bowline_validate")
    (param i32) (param i32) (param i32) (param i32)
    (result i32)
    i32.const 1))
"#,
        )
        .expect("wat parses");
        let (engine, module) = compiled_module(&module);

        let error = merge_with_wasm_plugin(
            &engine,
            &module,
            "data.bin",
            b"base",
            b"local",
            b"remote",
            WasmPluginLimits::default_with_output_limit(1),
        )
        .expect_err("oversized output is rejected");

        assert!(matches!(
            error,
            WasmMergeError::OutputTooLarge { len: 4, max: 1 }
        ));
    }

    #[test]
    fn wasm_plugin_nondeterministic_output_fails_safely() {
        let error = require_deterministic_output(Some(b"first".to_vec()), Some(b"second".to_vec()))
            .expect_err("different replay outputs are rejected");

        assert!(matches!(error, WasmMergeError::NonDeterministicOutput));
    }
}
