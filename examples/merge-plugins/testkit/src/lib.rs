use std::{
    env,
    error::Error,
    fmt,
    path::{Path, PathBuf},
};

const MERGE_EXPORT: &str = "bowline_merge";
const VALIDATE_EXPORT: &str = "bowline_validate";
const ALLOC_EXPORT: &str = "bowline_alloc";
const MEMORY_EXPORT: &str = "memory";
const WASM_FUEL: u64 = 5_000_000;
const WASM_MEMORY_MAX_BYTES: usize = 64 * 1024 * 1024;

pub struct ConformanceCase<'a> {
    pub path: &'a str,
    pub base: &'a [u8],
    pub local: &'a [u8],
    pub remote: &'a [u8],
    pub expected_merge: Option<&'a [u8]>,
}

pub fn built_wasm_path(manifest_dir: &str, wasm_file_stem: &str) -> PathBuf {
    let target_dir = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(manifest_dir).join("target"));
    target_dir
        .join("wasm32-unknown-unknown")
        .join("release")
        .join(format!("{wasm_file_stem}.wasm"))
}

pub fn round_trip(wasm_path: &Path, case: &ConformanceCase<'_>) -> Result<(), ConformanceError> {
    if !wasm_path.exists() {
        return Err(ConformanceError::ModuleMissing {
            path: wasm_path.to_path_buf(),
        });
    }

    let mut config = wasmtime::Config::new();
    config.consume_fuel(true);
    config.wasm_multi_memory(false);
    let engine = wasmtime::Engine::new(&config).map_err(ConformanceError::Runtime)?;
    let module = wasmtime::Module::from_file(&engine, wasm_path).map_err(ConformanceError::Runtime)?;

    // This harness replays the guest ABI from
    // crates/bowline-local/src/sync/merge_plugins/wasm.rs because the host
    // driver is crate-private while examples are detached workspaces. The ABI
    // export names and packed `(ptr << 32) | len` result are the contract until
    // Bowline adds the planned bowline_abi_version export.
    let mut store = wasmtime::Store::new(
        &engine,
        WasmStoreState {
            limits: wasmtime::StoreLimitsBuilder::new()
                .memory_size(WASM_MEMORY_MAX_BYTES)
                .memories(1)
                .tables(1)
                .instances(1)
                .trap_on_grow_failure(true)
                .build(),
        },
    );
    store.limiter(|state| &mut state.limits);
    store.set_fuel(WASM_FUEL).map_err(ConformanceError::Runtime)?;
    let instance =
        wasmtime::Instance::new(&mut store, &module, &[]).map_err(ConformanceError::Runtime)?;
    let memory = instance
        .get_memory(&mut store, MEMORY_EXPORT)
        .ok_or(ConformanceError::MissingExport(MEMORY_EXPORT))?;
    let merge = instance
        .get_typed_func::<(i32, i32, i32, i32, i32, i32, i32, i32), i64>(
            &mut store,
            MERGE_EXPORT,
        )
        .map_err(|_| ConformanceError::MissingExport(MERGE_EXPORT))?;
    let validate = instance
        .get_typed_func::<(i32, i32, i32, i32), i32>(&mut store, VALIDATE_EXPORT)
        .map_err(|_| ConformanceError::MissingExport(VALIDATE_EXPORT))?;
    let alloc = instance
        .get_typed_func::<i32, i32>(&mut store, ALLOC_EXPORT)
        .map_err(|_| ConformanceError::MissingExport(ALLOC_EXPORT))?;

    let input_len = input_byte_len(case)?;
    let input_ptr = alloc
        .call(&mut store, input_len as i32)
        .map_err(ConformanceError::Runtime)?;
    if input_ptr < 0 {
        return Err(ConformanceError::AllocationFailed);
    }
    let layout = InputLayout::new(input_ptr as usize, case)?;
    let memory_len = memory.data_size(&store);
    if layout.total_len > memory_len {
        return Err(ConformanceError::InputOutOfBounds {
            ptr: input_ptr as usize,
            len: input_len,
            memory_len,
        });
    }
    memory
        .write(&mut store, layout.base_ptr, case.base)
        .map_err(ConformanceError::Memory)?;
    memory
        .write(&mut store, layout.local_ptr, case.local)
        .map_err(ConformanceError::Memory)?;
    memory
        .write(&mut store, layout.remote_ptr, case.remote)
        .map_err(ConformanceError::Memory)?;
    memory
        .write(&mut store, layout.path_ptr, case.path.as_bytes())
        .map_err(ConformanceError::Memory)?;

    let result = merge
        .call(
            &mut store,
            (
                layout.base_ptr as i32,
                case.base.len() as i32,
                layout.local_ptr as i32,
                case.local.len() as i32,
                layout.remote_ptr as i32,
                case.remote.len() as i32,
                layout.path_ptr as i32,
                case.path.len() as i32,
            ),
        )
        .map_err(ConformanceError::Runtime)?;
    if result < 0 {
        return match case.expected_merge {
            Some(_) => Err(ConformanceError::UnexpectedDecline),
            None => Ok(()),
        };
    }
    let expected = case.expected_merge.ok_or(ConformanceError::UnexpectedMerge)?;
    let output_ptr = ((result as u64) >> 32) as usize;
    let output_len = (result as u64 & 0xffff_ffff) as usize;
    let memory_len = memory.data_size(&store);
    if output_ptr
        .checked_add(output_len)
        .is_none_or(|end| end > memory_len)
    {
        return Err(ConformanceError::OutputOutOfBounds {
            ptr: output_ptr,
            len: output_len,
            memory_len,
        });
    }
    let output = memory.data(&store)[output_ptr..output_ptr + output_len].to_vec();
    if output != expected {
        return Err(ConformanceError::UnexpectedOutput {
            expected_len: expected.len(),
            actual_len: output.len(),
        });
    }

    let valid = validate
        .call(
            &mut store,
            (
                output_ptr as i32,
                output_len as i32,
                layout.path_ptr as i32,
                case.path.len() as i32,
            ),
        )
        .map_err(ConformanceError::Runtime)?;
    if valid != 1 {
        return Err(ConformanceError::ValidationDeclined);
    }
    Ok(())
}

fn input_byte_len(case: &ConformanceCase<'_>) -> Result<usize, ConformanceError> {
    let len = case
        .base
        .len()
        .checked_add(case.local.len())
        .and_then(|len| len.checked_add(case.remote.len()))
        .and_then(|len| len.checked_add(case.path.len()))
        .ok_or(ConformanceError::InputTooLarge)?;
    if len > i32::MAX as usize {
        return Err(ConformanceError::InputTooLarge);
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
    fn new(base_offset: usize, case: &ConformanceCase<'_>) -> Result<Self, ConformanceError> {
        let base_ptr = base_offset;
        let local_ptr = base_ptr
            .checked_add(case.base.len())
            .ok_or(ConformanceError::InputTooLarge)?;
        let remote_ptr = local_ptr
            .checked_add(case.local.len())
            .ok_or(ConformanceError::InputTooLarge)?;
        let path_ptr = remote_ptr
            .checked_add(case.remote.len())
            .ok_or(ConformanceError::InputTooLarge)?;
        let total_len = path_ptr
            .checked_add(case.path.len())
            .ok_or(ConformanceError::InputTooLarge)?;
        if total_len > i32::MAX as usize {
            return Err(ConformanceError::InputTooLarge);
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
pub enum ConformanceError {
    ModuleMissing {
        path: PathBuf,
    },
    Runtime(wasmtime::Error),
    Memory(wasmtime::MemoryAccessError),
    MissingExport(&'static str),
    AllocationFailed,
    InputTooLarge,
    InputOutOfBounds {
        ptr: usize,
        len: usize,
        memory_len: usize,
    },
    OutputOutOfBounds {
        ptr: usize,
        len: usize,
        memory_len: usize,
    },
    UnexpectedDecline,
    UnexpectedMerge,
    UnexpectedOutput {
        expected_len: usize,
        actual_len: usize,
    },
    ValidationDeclined,
}

impl fmt::Display for ConformanceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ModuleMissing { path } => write!(
                formatter,
                "built Wasm module is missing at {}; build it first with `cargo build --target wasm32-unknown-unknown --release`",
                path.display()
            ),
            Self::Runtime(error) => write!(formatter, "{error}"),
            Self::Memory(error) => write!(formatter, "{error}"),
            Self::MissingExport(name) => write!(formatter, "missing WASM export `{name}`"),
            Self::AllocationFailed => formatter.write_str("WASM input allocation failed"),
            Self::InputTooLarge => formatter.write_str("WASM merge input is too large"),
            Self::InputOutOfBounds {
                ptr,
                len,
                memory_len,
            } => write!(
                formatter,
                "WASM input pointer {ptr} plus length {len} exceeds memory {memory_len}"
            ),
            Self::OutputOutOfBounds {
                ptr,
                len,
                memory_len,
            } => write!(
                formatter,
                "WASM output pointer {ptr} plus length {len} exceeds memory {memory_len}"
            ),
            Self::UnexpectedDecline => {
                formatter.write_str("WASM merge declined but expected merged bytes")
            }
            Self::UnexpectedMerge => {
                formatter.write_str("WASM merge returned bytes but expected a decline")
            }
            Self::UnexpectedOutput {
                expected_len,
                actual_len,
            } => write!(
                formatter,
                "WASM merge output differed from expected bytes: expected {expected_len} bytes, got {actual_len}"
            ),
            Self::ValidationDeclined => {
                formatter.write_str("WASM validate rejected the merge output")
            }
        }
    }
}

impl Error for ConformanceError {}
