// Copyright 2023 Vivek Panyam
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{collections::HashMap, sync::Arc};

use carton::{
    types::{for_each_carton_type, Device, GenericStorage, LoadOpts, Tensor, TypedStorage},
    Carton,
};
use ndarray::ShapeBuilder;
use neon::{prelude::*, types::buffer::TypedArray};
use once_cell::sync::OnceCell;
use tokio::runtime::Runtime;

struct CartonWrapper(pub Arc<Carton>);

impl Finalize for CartonWrapper {}

// Return a global tokio runtime or create one if it doesn't exist.
// Throws a JavaScript exception if the `Runtime` fails to create.
// Based on https://github.com/neon-bindings/examples/blob/main/examples/tokio-fetch/src/lib.rs
fn runtime<'a, C: Context<'a>>(cx: &mut C) -> NeonResult<&'static Runtime> {
    static RUNTIME: OnceCell<Runtime> = OnceCell::new();

    RUNTIME.get_or_try_init(|| Runtime::new().or_else(|err| cx.throw_error(err.to_string())))
}

/// Load a carton model
fn load(mut cx: FunctionContext) -> JsResult<JsPromise> {
    let load_opts = cx.argument::<JsObject>(0)?;

    // TODO: refactor this
    let path = load_opts
        .get::<JsString, _, _>(&mut cx, "path")?
        .value(&mut cx);
    let override_runner_name = load_opts
        .get_opt::<JsString, _, _>(&mut cx, "override_runner_name")?
        .map(|item| item.value(&mut cx));
    let override_required_framework_version = load_opts
        .get_opt::<JsString, _, _>(&mut cx, "override_required_framework_version")?
        .map(|item| item.value(&mut cx));

    // TODO: handle load options
    // let override_runner_opts = load_opts
    //     .get_opt::<JsString, _, _>(&mut cx, "override_runner_opts")?
    //     .map(|item| item.value(&mut cx));

    let visible_device = load_opts
        .get::<JsString, _, _>(&mut cx, "visible_device")?
        .value(&mut cx);

    let opts = LoadOpts {
        override_runner_name,
        override_required_framework_version,
        override_runner_opts: None,
        visible_device: Device::maybe_from_str(&visible_device)
            .or_else(|err| cx.throw_error(err.to_string()))?,
    };

    let rt = runtime(&mut cx)?;
    let channel = cx.channel();

    // Create a promise
    let (deferred, promise) = cx.promise();

    // Spawn a task to create a new client
    rt.spawn(async move {
        // Load the model
        let carton = Carton::load(path, opts).await;

        // This runs on the JS main thread
        deferred.settle_with(&channel, move |mut cx| {
            let carton = carton.or_else(|err| cx.throw_error(err.to_string()))?;

            // let model_name = cx.string(&carton.model_name);
            // let model_runner = cx.string(&carton.model_runner);

            let handle = cx.boxed(CartonWrapper(Arc::new(carton)));

            let out = cx.empty_object();
            out.set(&mut cx, "handle", handle)?;
            // out.set(&mut cx, "name", model_name)?;
            // out.set(&mut cx, "runner", model_runner)?;

            Ok(out)
        });
    });

    // Return the promise to js
    Ok(promise)
}

impl CartonWrapper {
    /// The first arg should be a map from strings (tensor names) to objects in the below structure:
    /// {
    ///     "buffer": ArrayBuffer,
    ///     "shape": [1, 2, 3],
    ///     "dtype": "float32",
    ///     "stride": [...]
    /// }
    ///
    fn infer(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let tensors_js = cx.argument::<JsObject>(0)?;
        let mut tensors = HashMap::new();

        // Get all the keys and values
        let props = tensors_js
            .get_own_property_names(&mut cx)?
            .to_vec(&mut cx)?;

        // Convert to Tensor
        for prop in props {
            let val = tensors_js.get::<JsObject, _, _>(&mut cx, prop)?;

            // Get the buffer, shape, stride, and dtype
            let jsbuffer = val.get::<JsArrayBuffer, _, _>(&mut cx, "buffer")?;

            // TODO this makes a copy
            // Doing this for now to avoid some mutable borrow issues
            let buffer = jsbuffer.as_slice(&mut cx).to_vec();

            let shape: Vec<usize> = val
                .get::<JsArray, _, _>(&mut cx, "shape")?
                .to_vec(&mut cx)?
                .iter()
                .map(|item| {
                    item.downcast_or_throw::<JsNumber, _>(&mut cx)
                        .unwrap()
                        .value(&mut cx) as usize
                })
                .collect();

            let stride: Vec<usize> = val
                .get::<JsArray, _, _>(&mut cx, "stride")?
                .to_vec(&mut cx)?
                .iter()
                .map(|item| {
                    item.downcast_or_throw::<JsNumber, _>(&mut cx)
                        .unwrap()
                        .value(&mut cx) as usize
                })
                .collect();

            let dtype = val.get::<JsString, _, _>(&mut cx, "dtype")?.value(&mut cx);

            // TODO this makes another copy (the `to_owned`)
            // TODO: we should ignore strings here
            for_each_carton_type! {
                let t: Tensor<GenericStorage> = match dtype.as_str() {
                    $(
                        $TypeStr => unsafe {
                            Tensor::$CartonType(ndarray::ArrayView::from_shape_ptr(
                                shape.strides(stride),
                                buffer.as_ptr() as *const $RustType,
                            ).to_owned())
                        },
                    )*
                    dtype => panic!("Got unknown dtype: {dtype}"),
                };

                // For some reason, this needs to go inside the macro call
                tensors.insert(prop.downcast_or_throw::<JsString, _>(&mut cx)?.value(&mut cx), t);
            }
        }

        let this = cx
            .this()
            .downcast_or_throw::<JsBox<CartonWrapper>, _>(&mut cx)?
            .0
            .clone();

        // Get the tokio runtime
        let rt = runtime(&mut cx)?;
        let channel = cx.channel();

        // Create a promise
        let (deferred, promise) = cx.promise();

        // Spawn a task
        rt.spawn(async move {
            let res = this.infer(tensors).await;

            // This runs on the JS main thread
            deferred.settle_with(&channel, move |mut cx| {
                let res = res.or_else(|err| cx.throw_error(err.to_string()))?;

                // Convert the outputs
                let out = cx.empty_object();
                for (k, v) in res {
                    // TODO: this should ignore the `string` type
                    for_each_carton_type! {
                        match v {
                            $(
                                Tensor::$CartonType(t) => {
                                    // Get the data as a slice
                                    // TODO: this can make a copy
                                    let view = t.view();
                                    let mut standard = view.as_standard_layout();

                                    let data = standard.as_slice_mut().unwrap();

                                    // View it as a u8 slice
                                    let data = unsafe {
                                        std::slice::from_raw_parts_mut(
                                            data.as_mut_ptr() as *mut u8,
                                            data.len() * std::mem::size_of::<$RustType>(),
                                        )
                                    };

                                    let buf = JsArrayBuffer::external(&mut cx, data);

                                    // Get the shape
                                    let shape = vec_to_array(&mut cx, view.shape())?;

                                    let typestr = cx.string($TypeStr);
                                    let keystr = cx.string(k);

                                    // Put all the info in an object
                                    let info = cx.empty_object();
                                    info.set(&mut cx, "buffer", buf)?;
                                    info.set(&mut cx, "dtype", typestr)?;
                                    info.set(&mut cx, "shape", shape)?;
                                    out.set(&mut cx, keystr, info)?;
                                },
                            )*
                            Tensor::NestedTensor(_) => panic!("Nested tensor output not implemented yet"),
                        }

                    }
                }

                Ok(out)
            });
        });

        // Return the promise to node
        Ok(promise)
    }
}

#[neon::main]
fn main(mut cx: ModuleContext) -> NeonResult<()> {
    cx.export_function("load", load)?;
    cx.export_function("infer", CartonWrapper::infer)?;
    Ok(())
}

// Based on https://neon-bindings.com/docs/arrays
// TODO: we could probably make this generic on the data type
fn vec_to_array<'a, C: Context<'a>>(cx: &mut C, data: &[usize]) -> JsResult<'a, JsArray> {
    let a = JsArray::new(cx, data.len() as u32);

    for (i, s) in data.iter().enumerate() {
        let v = cx.number(*s as u32);
        a.set(cx, i as u32, v)?;
    }

    Ok(a)
}
