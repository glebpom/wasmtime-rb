use super::errors::wasi_exit_error;
use super::{engine::Engine, func::Caller, root, trap::Trap, wasi_ctx_builder::WasiCtxBuilder};
use crate::{error, helpers::WrappedStruct};
use magnus::Class;
use magnus::{
    exception::Exception, function, method, scan_args, value::BoxValue, DataTypeFunctions, Error,
    Module, Object, TypedData, Value, QNIL,
};
use std::cell::{RefCell, UnsafeCell};
use std::convert::TryFrom;
use wasmtime::{AsContext, AsContextMut, Store as StoreImpl, StoreContext, StoreContextMut};
use wasmtime_wasi::{I32Exit, WasiCtx};

pub struct StoreData {
    user_data: Value,
    host_exception: HostException,
    pub wasi: Option<WasiCtx>,
}

type BoxedException = BoxValue<Exception>;
#[derive(Debug, Default)]
pub struct HostException(Option<BoxedException>);
impl HostException {
    pub fn take(&mut self) -> Option<Exception> {
        std::mem::take(&mut self.0).map(|e| e.to_owned())
    }

    pub fn hold(&mut self, e: Exception) {
        self.0 = Some(BoxValue::new(e));
    }
}

impl StoreData {
    pub fn exception(&mut self) -> &mut HostException {
        &mut self.host_exception
    }

    pub fn take_last_error(&mut self) -> Option<Error> {
        self.host_exception.take().map(Error::from)
    }

    pub fn user_data(&self) -> Value {
        self.user_data
    }

    pub fn has_wasi_ctx(&self) -> bool {
        self.wasi.is_some()
    }

    pub fn wasi_ctx_mut(&mut self) -> &mut WasiCtx {
        self.wasi.as_mut().expect("Store must have a WASI context")
    }
}

/// @yard
/// Represents a WebAssebmly store.
/// @see https://docs.rs/wasmtime/latest/wasmtime/struct.Store.html Wasmtime's Rust doc
#[derive(Debug, TypedData)]
#[magnus(class = "Wasmtime::Store", size, mark, free_immediatly)]
pub struct Store {
    inner: UnsafeCell<StoreImpl<StoreData>>,
    refs: RefCell<Vec<Value>>,
}

impl DataTypeFunctions for Store {
    fn mark(&self) {
        self.refs.borrow().iter().for_each(magnus::gc::mark);
    }
}

unsafe impl Send for Store {}
unsafe impl Send for StoreData {}

impl Store {
    /// @yard
    ///
    /// @def new(engine, data = nil)
    /// @param engine [Wasmtime::Engine]
    ///   The engine for this store.
    /// @param data [Object]
    ///   The data attached to the store. Can be retrieved through {Wasmtime::Store#data} and {Wasmtime::Caller#data}.
    /// @return [Wasmtime::Store]
    ///
    /// @example
    ///   store = Wasmtime::Store.new(Wasmtime::Engine.new)
    ///
    /// @example
    ///   store = Wasmtime::Store.new(Wasmtime::Engine.new, {})
    pub fn new(args: &[Value]) -> Result<Self, Error> {
        let args = scan_args::scan_args::<(&Engine,), (Option<Value>,), (), (), (), ()>(args)?;
        let (engine,) = args.required;
        let (user_data,) = args.optional;
        let user_data = user_data.unwrap_or_else(|| QNIL.into());

        let eng = engine.get();
        let store_data = StoreData {
            user_data,
            host_exception: HostException::default(),
            wasi: None,
        };
        let store = Self {
            inner: UnsafeCell::new(StoreImpl::new(eng, store_data)),
            refs: Default::default(),
        };

        store.retain(user_data);

        Ok(store)
    }

    /// @yard
    /// @return [Object] The passed in value in {.new}
    pub fn data(&self) -> Value {
        self.context().data().user_data()
    }

    /// @yard
    /// Configure the WASI context on the store.
    /// @def configure_wasi(wasi_config)
    /// @param wasi_config [WasiCtxBuilder]
    /// @return [Store] +self+
    pub fn configure_wasi(
        rb_self: WrappedStruct<Self>,
        wasi_config: &WasiCtxBuilder,
    ) -> Result<WrappedStruct<Self>, Error> {
        rb_self.get()?.context_mut().data_mut().wasi = Some(wasi_config.build_context()?);

        Ok(rb_self)
    }

    pub fn context(&self) -> StoreContext<StoreData> {
        unsafe { (*self.inner.get()).as_context() }
    }

    pub fn context_mut(&self) -> StoreContextMut<StoreData> {
        unsafe { (*self.inner.get()).as_context_mut() }
    }

    pub fn retain(&self, value: Value) {
        self.refs.borrow_mut().push(value);
    }
}

/// A wrapper around a Ruby Value that has a store context.
/// Used in places where both Store or Caller can be used.
#[derive(Debug, Clone, Copy)]
pub enum StoreContextValue<'a> {
    Store(WrappedStruct<Store>),
    Caller(WrappedStruct<Caller<'a>>),
}

impl<'a> From<WrappedStruct<Store>> for StoreContextValue<'a> {
    fn from(store: WrappedStruct<Store>) -> Self {
        StoreContextValue::Store(store)
    }
}

impl<'a> From<WrappedStruct<Caller<'a>>> for StoreContextValue<'a> {
    fn from(caller: WrappedStruct<Caller<'a>>) -> Self {
        StoreContextValue::Caller(caller)
    }
}

impl<'a> StoreContextValue<'a> {
    pub fn mark(&self) {
        match self {
            Self::Store(store) => store.mark(),
            Self::Caller(_) => {
                // The Caller is on the stack while it's "live". Right before the end of a host call,
                // we remove the Caller form the Ruby object, thus there is no need to mark.
            }
        }
    }

    pub fn context(&self) -> Result<StoreContext<StoreData>, Error> {
        match self {
            Self::Store(store) => Ok(store.get()?.context()),
            Self::Caller(caller) => caller.get()?.context(),
        }
    }

    pub fn context_mut(&self) -> Result<StoreContextMut<StoreData>, Error> {
        match self {
            Self::Store(store) => Ok(store.get()?.context_mut()),
            Self::Caller(caller) => caller.get()?.context_mut(),
        }
    }

    pub fn handle_wasm_error(&self, error: anyhow::Error) -> Error {
        match self.context_mut() {
            Ok(mut context) => context.data_mut().take_last_error().unwrap_or_else(|| {
                if let Some(exit) = error.downcast_ref::<I32Exit>() {
                    wasi_exit_error().new_instance((exit.0,)).unwrap().into()
                } else {
                    Trap::try_from(error)
                        .map(|trap| trap.into())
                        .unwrap_or_else(|e| error!("{}", e))
                }
            }),
            Err(e) => e,
        }
    }
}

pub fn init() -> Result<(), Error> {
    let class = root().define_class("Store", Default::default())?;

    class.define_singleton_method("new", function!(Store::new, -1))?;
    class.define_method("data", method!(Store::data, 0))?;
    class.define_method("configure_wasi", method!(Store::configure_wasi, 1))?;

    Ok(())
}
