use super::wasm_instance_env::WasmInstanceEnv;
use super::Mem;
use crate::host::instance_env::InstanceEnv;
use crate::host::wasm_common::module_host_actor::{DescribeError, InitializationError};
use crate::host::wasm_common::*;
use crate::host::{EnergyQuanta, Timestamp};
use bytes::Bytes;
use spacetimedb_lib::{Address, Identity};
use wasmer::{
    imports, AsStoreMut, Engine, ExternType, Function, FunctionEnv, Imports, Instance, Module, RuntimeError, Store,
    TypedFunction,
};
use wasmer_middlewares::metering as wasmer_metering;

fn get_remaining_points(ctx: &mut impl AsStoreMut, instance: &Instance) -> u64 {
    let remaining_points = wasmer_metering::get_remaining_points(ctx, instance);
    match remaining_points {
        wasmer_metering::MeteringPoints::Remaining(x) => x,
        wasmer_metering::MeteringPoints::Exhausted => 0,
    }
}

fn log_traceback(func_type: &str, func: &str, e: &RuntimeError) {
    let frames = e.trace();
    let frames_len = frames.len();

    log::info!("{} \"{}\" runtime error: {}", func_type, func, e.message());
    for (i, frame) in frames.iter().enumerate().take(frames_len) {
        log::info!(
            "  Frame #{}: {:?}::{}",
            frames_len - i,
            frame.module_name(),
            rustc_demangle::demangle(frame.function_name().unwrap_or("<func>"))
        );
    }
}

#[derive(Clone)]
pub struct WasmerModule {
    module: Module,
    engine: Engine,
}

impl WasmerModule {
    pub fn new(module: Module, engine: Engine) -> Self {
        WasmerModule { module, engine }
    }

    pub const IMPLEMENTED_ABI: abi::VersionTuple = abi::VersionTuple::new(7, 0);

    fn imports(&self, store: &mut Store, env: &FunctionEnv<WasmInstanceEnv>) -> Imports {
        #[allow(clippy::assertions_on_constants)]
        const _: () = assert!(WasmerModule::IMPLEMENTED_ABI.major == spacetimedb_lib::MODULE_ABI_MAJOR_VERSION);
        imports! {
            "spacetime_7.0" => {
                "_schedule_reducer" => Function::new_typed_with_env(store, env, WasmInstanceEnv::schedule_reducer),
                "_cancel_reducer" => Function::new_typed_with_env(store, env, WasmInstanceEnv::cancel_reducer),
                "_delete_by_col_eq" => Function::new_typed_with_env(
                    store,
                    env,
                    WasmInstanceEnv::delete_by_col_eq,
                ),
                "_insert" => Function::new_typed_with_env(
                    store,
                    env,
                    WasmInstanceEnv::insert,
                ),
                "_get_table_id" => Function::new_typed_with_env(
                    store,
                    env,
                    WasmInstanceEnv::get_table_id,
                ),
                "_create_index" => Function::new_typed_with_env(
                    store,
                    env,
                    WasmInstanceEnv::create_index,
                ),
                "_iter_by_col_eq" => Function::new_typed_with_env(
                    store,
                    env,
                    WasmInstanceEnv::iter_by_col_eq,
                ),
                "_iter_start" => Function::new_typed_with_env(
                    store,
                    env,
                    WasmInstanceEnv::iter_start
                ),
                "_iter_start_filtered" => Function::new_typed_with_env(
                    store,
                    env,
                    WasmInstanceEnv::iter_start_filtered
                ),
                "_iter_next" => Function::new_typed_with_env(
                    store,
                    env,
                    WasmInstanceEnv::iter_next
                ),
                "_iter_drop" => Function::new_typed_with_env(
                    store,
                    env,
                    WasmInstanceEnv::iter_drop
                ),
                "_console_log" => Function::new_typed_with_env(
                    store,
                    env,
                    WasmInstanceEnv::console_log
                ),
                "_buffer_len" => Function::new_typed_with_env(store, env, WasmInstanceEnv::buffer_len),
                "_buffer_consume" => Function::new_typed_with_env(store, env, WasmInstanceEnv::buffer_consume),
                "_buffer_alloc" => Function::new_typed_with_env(store, env, WasmInstanceEnv::buffer_alloc),
                "_span_start" => Function::new_typed_with_env(store, env, WasmInstanceEnv::span_start),
                "_span_end" => Function::new_typed_with_env(store, env, WasmInstanceEnv::span_end),
            }
        }
    }
}

impl module_host_actor::WasmModule for WasmerModule {
    type Instance = WasmerInstance;
    type InstancePre = Self;

    type ExternType = ExternType;

    fn get_export(&self, s: &str) -> Option<Self::ExternType> {
        self.module
            .exports()
            .find(|exp| exp.name() == s)
            .map(|exp| exp.ty().clone())
    }

    fn for_each_export<E>(&self, mut f: impl FnMut(&str, &Self::ExternType) -> Result<(), E>) -> Result<(), E> {
        self.module.exports().try_for_each(|exp| f(exp.name(), exp.ty()))
    }

    fn instantiate_pre(&self) -> Result<Self::InstancePre, InitializationError> {
        Ok(self.clone())
    }
}

impl module_host_actor::WasmInstancePre for WasmerModule {
    type Instance = WasmerInstance;

    fn instantiate(&self, env: InstanceEnv, func_names: &FuncNames) -> Result<Self::Instance, InitializationError> {
        let mut store = Store::new(self.engine.clone());
        let env = WasmInstanceEnv::new(env);
        let env = FunctionEnv::new(&mut store, env);
        let imports = self.imports(&mut store, &env);
        let instance = Instance::new(&mut store, &self.module, &imports)
            .map_err(|err| InitializationError::Instantiation(err.into()))?;

        let mem = Mem::extract(&instance.exports).unwrap();

        env.as_mut(&mut store).instantiate(mem);

        // Note: this budget is just for initializers
        let budget = EnergyQuanta::DEFAULT_BUDGET.as_points();
        wasmer_metering::set_remaining_points(&mut store, &instance, budget);

        for preinit in &func_names.preinits {
            let func = instance.exports.get_typed_function::<(), ()>(&store, preinit).unwrap();
            func.call(&mut store).map_err(|err| InitializationError::RuntimeError {
                err: err.into(),
                func: preinit.clone(),
            })?;
        }

        let init = instance.exports.get_typed_function::<(), u32>(&store, SETUP_DUNDER);
        if let Ok(init) = init {
            match init.call(&mut store).map(BufferIdx) {
                Ok(errbuf) if errbuf.is_invalid() => {}
                Ok(errbuf) => {
                    let errbuf = env
                        .as_mut(&mut store)
                        .take_buffer(errbuf)
                        .unwrap_or_else(|| "unknown error".as_bytes().into());
                    let errbuf = crate::util::string_from_utf8_lossy_owned(errbuf.into()).into();
                    // TODO: catch this and return the error message to the http client
                    return Err(InitializationError::Setup(errbuf));
                }
                Err(err) => {
                    return Err(InitializationError::RuntimeError {
                        err: err.into(),
                        func: SETUP_DUNDER.to_owned(),
                    });
                }
            }
        }

        Ok(WasmerInstance { store, env, instance })
    }
}

pub struct WasmerInstance {
    store: Store,
    env: FunctionEnv<WasmInstanceEnv>,
    instance: Instance,
}

impl WasmerInstance {
    fn call_describer(&mut self, describer: &Function, describer_func_name: &str) -> Result<Bytes, DescribeError> {
        let start = std::time::Instant::now();
        log::trace!("Start describer \"{}\"...", describer_func_name);

        let store = &mut self.store;
        let describer = describer
            .typed::<(), u32>(store)
            .map_err(|_| DescribeError::Signature)?;
        let result = describer.call(store).map(BufferIdx);
        let duration = start.elapsed();
        log::trace!("Describer \"{}\" ran: {} us", describer_func_name, duration.as_micros(),);
        let buf = result.map_err(|err| {
            log_traceback("describer", describer_func_name, &err);
            DescribeError::RuntimeError(err.into())
        })?;
        let bytes = self
            .env
            .as_mut(store)
            .take_buffer(buf)
            .ok_or(DescribeError::BadBuffer)?;

        // Clear all of the instance state associated to this describer call.
        self.env.as_mut(store).finish_reducer();

        Ok(bytes)
    }
}

impl module_host_actor::WasmInstance for WasmerInstance {
    fn extract_descriptions(&mut self) -> Result<Bytes, DescribeError> {
        let describer = self.instance.exports.get_function(DESCRIBE_MODULE_DUNDER).unwrap();
        let describer = describer.clone();
        self.call_describer(&describer, DESCRIBE_MODULE_DUNDER)
    }

    fn instance_env(&self) -> &InstanceEnv {
        self.env.as_ref(&self.store).instance_env()
    }

    type Trap = wasmer::RuntimeError;

    fn call_reducer(
        &mut self,
        reducer_id: usize,
        budget: EnergyQuanta,
        sender_identity: &Identity,
        sender_address: &Address,
        timestamp: Timestamp,
        arg_bytes: Bytes,
    ) -> module_host_actor::ExecuteResult<Self::Trap> {
        self.call_tx_function::<(u32, u32, u32, u64, u32), 3>(
            CALL_REDUCER_DUNDER,
            budget,
            [
                Bytes::copy_from_slice(sender_identity.as_bytes()),
                Bytes::copy_from_slice(sender_address.as_slice()),
                arg_bytes,
            ],
            |func, store, [sender_identity, sender_address, args]| {
                func.call(
                    store,
                    reducer_id as u32,
                    sender_identity.0,
                    sender_address.0,
                    timestamp.0,
                    args.0,
                )
            },
        )
    }

    fn log_traceback(func_type: &str, func: &str, trap: &Self::Trap) {
        log_traceback(func_type, func, trap)
    }
}

impl WasmerInstance {
    fn call_tx_function<Args: wasmer::WasmTypeList, const N_BUFS: usize>(
        &mut self,
        reducer_symbol: &str,
        budget: EnergyQuanta,
        bufs: [Bytes; N_BUFS],
        // would be nicer if there was a TypedFunction::call_tuple(&self, store, ArgsTuple)
        call: impl FnOnce(TypedFunction<Args, u32>, &mut Store, [BufferIdx; N_BUFS]) -> Result<u32, RuntimeError>,
    ) -> module_host_actor::ExecuteResult<RuntimeError> {
        let store = &mut self.store;
        let instance = &self.instance;
        let budget = budget.as_points();
        wasmer_metering::set_remaining_points(store, instance, budget);

        let reduce = instance
            .exports
            .get_typed_function::<Args, u32>(store, reducer_symbol)
            .expect("invalid reducer");

        let bufs = bufs.map(|data| self.env.as_mut(store).insert_buffer(data));

        self.env.as_mut(store).start_reducer();

        // pass ownership of the `ptr` allocation into the reducer
        let result = call(reduce, store, bufs).and_then(|errbuf| {
            let errbuf = BufferIdx(errbuf);
            Ok(if errbuf.is_invalid() {
                Ok(())
            } else {
                let errmsg = self
                    .env
                    .as_mut(store)
                    .take_buffer(errbuf)
                    .ok_or_else(|| RuntimeError::new("invalid buffer handle"))?;
                Err(crate::util::string_from_utf8_lossy_owned(errmsg.into()).into())
            })
        });

        // Signal that this reducer call is finished. This gets us the timings
        // associated to our reducer call, and clears all of the instance state
        // associated to the call.
        let timings = self.env.as_mut(store).finish_reducer();

        let remaining = get_remaining_points(store, instance);
        let energy = module_host_actor::EnergyStats {
            used: EnergyQuanta::from_points(budget) - EnergyQuanta::from_points(remaining),
            remaining: EnergyQuanta::from_points(remaining),
        };

        module_host_actor::ExecuteResult {
            energy,
            timings,
            call_result: result,
        }
    }
}
