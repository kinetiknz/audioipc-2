// Copyright © 2017 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details

use crate::ClientContext;
use crate::{assert_not_in_callback, run_in_callback};
use audioipc::rpc;
use audioipc::shm::SharedMem;
use audioipc::{codec::LengthDelimitedCodec, messages::StreamCreateParams};
use audioipc::{
    messages::{self, CallbackReq, CallbackResp, ClientMessage, ServerMessage},
    platformhandle_passing::{framed_with_platformhandles, FramedWithPlatformHandles},
};
use cubeb_backend::{ffi, DeviceRef, Error, Result, Stream, StreamOps};
use futures::Future;
use futures_cpupool::{CpuFuture, CpuPool};
use std::os::raw::c_void;
use std::ptr;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::{
    convert::TryInto,
    ffi::{CStr, CString},
    time::{Duration, Instant},
};
use tokio::reactor;

pub struct Device(ffi::cubeb_device);

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            if !self.0.input_name.is_null() {
                let _ = CString::from_raw(self.0.input_name as *mut _);
            }
            if !self.0.output_name.is_null() {
                let _ = CString::from_raw(self.0.output_name as *mut _);
            }
        }
    }
}

// ClientStream's layout *must* match cubeb.c's `struct cubeb_stream` for the
// common fields.
#[repr(C)]
#[derive(Debug)]
pub struct ClientStream<'ctx> {
    // This must be a reference to Context for cubeb, cubeb accesses
    // stream methods via stream->context->ops
    context: &'ctx ClientContext,
    user_ptr: *mut c_void,
    token: usize,
    device_change_cb: Arc<Mutex<ffi::cubeb_device_changed_callback>>,
    // Signals ClientStream that CallbackServer has dropped.
    shutdown_rx: mpsc::Receiver<()>,
    stream_output_rate: Option<u32>,
    cached_position: Option<(u64, Instant)>,
    cached_calls: (u64, u64),
}

struct CallbackServer {
    shm: Option<SharedMem>,
    input: Option<Vec<u8>>,
    data_cb: ffi::cubeb_data_callback,
    state_cb: ffi::cubeb_state_callback,
    user_ptr: usize,
    cpu_pool: CpuPool,
    device_change_cb: Arc<Mutex<ffi::cubeb_device_changed_callback>>,
    // Signals ClientStream that CallbackServer has dropped.
    _shutdown_tx: mpsc::Sender<()>,
}

impl rpc::Server for CallbackServer {
    type Request = CallbackReq;
    type Response = CallbackResp;
    type Future = CpuFuture<Self::Response, ()>;
    type Transport = FramedWithPlatformHandles<
        audioipc::AsyncMessageStream,
        LengthDelimitedCodec<Self::Response, Self::Request>,
    >;

    fn process(&mut self, req: Self::Request) -> Self::Future {
        match req {
            CallbackReq::Data {
                nframes,
                input_frame_size,
                output_frame_size,
            } => {
                trace!(
                    "stream_thread: Data Callback: nframes={} input_fs={} output_fs={}",
                    nframes,
                    input_frame_size,
                    output_frame_size,
                );

                // Clone values that need to be moved into the cpu pool thread.
                let mut shm = unsafe { self.shm.as_ref().unwrap().unsafe_view() };
                let input_copy_ptr = match &mut self.input {
                    Some(buf) => {
                        assert!(input_frame_size > 0);
                        assert!(buf.capacity() >= nframes as usize * input_frame_size);
                        buf.as_mut_ptr()
                    }
                    None => ptr::null_mut(),
                } as usize;
                let user_ptr = self.user_ptr;
                let cb = self.data_cb.unwrap();

                self.cpu_pool.spawn_fn(move || {
                    // Input and output reuse the same shmem backing.
                    // cubeb's data_callback isn't specified strongly
                    // enough that it requires the data_callback
                    // callee to consume all of the input before
                    // writing to the output.  That means we need to
                    // copy the input here.
                    if input_copy_ptr != 0 {
                        unsafe {
                            let input = shm.get_slice(nframes as usize * input_frame_size).unwrap();
                            ptr::copy_nonoverlapping(
                                input.as_ptr(),
                                input_copy_ptr as *mut _,
                                input.len(),
                            );
                        }
                    }
                    let output_ptr = if output_frame_size != 0 {
                        unsafe {
                            shm.get_mut_slice(nframes as usize * output_frame_size)
                                .unwrap()
                                .as_mut_ptr()
                        }
                    } else {
                        ptr::null_mut()
                    };

                    run_in_callback(|| {
                        let nframes = unsafe {
                            cb(
                                ptr::null_mut(), // https://github.com/kinetiknz/cubeb/issues/518
                                user_ptr as *mut c_void,
                                input_copy_ptr as *const _,
                                output_ptr as *mut _,
                                nframes as _,
                            )
                        };

                        Ok(CallbackResp::Data(nframes as isize))
                    })
                })
            }
            CallbackReq::State(state) => {
                trace!("stream_thread: State Callback: {:?}", state);
                let user_ptr = self.user_ptr;
                let cb = self.state_cb.unwrap();
                self.cpu_pool.spawn_fn(move || {
                    run_in_callback(|| unsafe {
                        cb(ptr::null_mut(), user_ptr as *mut _, state);
                    });

                    Ok(CallbackResp::State)
                })
            }
            CallbackReq::DeviceChange => {
                let cb = self.device_change_cb.clone();
                let user_ptr = self.user_ptr;
                self.cpu_pool.spawn_fn(move || {
                    run_in_callback(|| {
                        let cb = cb.lock().unwrap();
                        if let Some(cb) = *cb {
                            unsafe {
                                cb(user_ptr as *mut _);
                            }
                        } else {
                            warn!("DeviceChange received with null callback");
                        }
                    });

                    Ok(CallbackResp::DeviceChange)
                })
            }
            CallbackReq::SharedMem(mut handle) => {
                let shm = unsafe {
                    SharedMem::from(handle.local_handle.take().unwrap(), audioipc::SHM_AREA_SIZE)
                        .expect("Client failed to set up shmem")
                };
                self.shm = Some(shm);
                self.cpu_pool.spawn_fn(move || Ok(CallbackResp::SharedMem))
            }
        }
    }
}

impl<'ctx> ClientStream<'ctx> {
    fn init(
        ctx: &'ctx ClientContext,
        init_params: messages::StreamInitParams,
        data_callback: ffi::cubeb_data_callback,
        state_callback: ffi::cubeb_state_callback,
        user_ptr: *mut c_void,
    ) -> Result<Stream> {
        assert_not_in_callback();

        let rpc = ctx.rpc();
        let stream_output_rate = init_params.output_stream_params.map(|p| p.rate);
        let create_params = StreamCreateParams {
            input_stream_params: init_params.input_stream_params,
            output_stream_params: init_params.output_stream_params,
        };
        let mut data = send_recv!(rpc, StreamCreate(create_params) => StreamCreated())?;

        debug!(
            "token = {}, handle = {:?}",
            data.token, data.platform_handle
        );

        let stream = unsafe {
            audioipc::MessageStream::from_raw_fd(
                data.platform_handle.local_handle.take().unwrap().into_raw(),
            )
        };

        let input = if init_params.input_stream_params.is_some() {
            Some(Vec::with_capacity(audioipc::SHM_AREA_SIZE))
        } else {
            None
        };

        let user_data = user_ptr as usize;

        let cpu_pool = ctx.cpu_pool();

        let null_cb: ffi::cubeb_device_changed_callback = None;
        let device_change_cb = Arc::new(Mutex::new(null_cb));

        let (_shutdown_tx, shutdown_rx) = mpsc::channel();

        let server = CallbackServer {
            shm: None,
            input,
            data_cb: data_callback,
            state_cb: state_callback,
            user_ptr: user_data,
            cpu_pool,
            device_change_cb: device_change_cb.clone(),
            _shutdown_tx,
        };

        let (wait_tx, wait_rx) = mpsc::channel();
        ctx.handle()
            .spawn(futures::future::lazy(move || {
                let handle = reactor::Handle::default();
                let stream = stream.into_tokio_ipc(&handle).unwrap();
                let transport = framed_with_platformhandles(stream, Default::default());
                rpc::bind_server(transport, server);
                wait_tx.send(()).unwrap();
                Ok(())
            }))
            .expect("Failed to spawn CallbackServer");
        wait_rx.recv().unwrap();

        send_recv!(rpc, StreamInit(data.token, init_params) => StreamInitialized)?;

        let stream = Box::into_raw(Box::new(ClientStream {
            context: ctx,
            user_ptr,
            token: data.token,
            device_change_cb,
            shutdown_rx,
            stream_output_rate,
            cached_position: None,
            cached_calls: (0, 0),
        }));
        Ok(unsafe { Stream::from_ptr(stream as *mut _) })
    }
}

impl<'ctx> Drop for ClientStream<'ctx> {
    fn drop(&mut self) {
        eprintln!(
            "ClientStream cached {}/{} get_position calls",
            self.cached_calls.0, self.cached_calls.1
        );
        debug!("ClientStream drop");
        let rpc = self.context.rpc();
        let _ = send_recv!(rpc, StreamDestroy(self.token) => StreamDestroyed);
        debug!("ClientStream drop - stream destroyed");
        // Wait for CallbackServer to shutdown.  The remote server drops the RPC
        // connection during StreamDestroy, which will cause CallbackServer to drop
        // once the connection close is detected.  Dropping CallbackServer will
        // cause the shutdown channel to error on recv, which we rely on to
        // synchronize with CallbackServer dropping.
        let _ = self.shutdown_rx.recv();
        debug!("ClientStream dropped");
    }
}

impl<'ctx> StreamOps for ClientStream<'ctx> {
    fn start(&mut self) -> Result<()> {
        assert_not_in_callback();
        let rpc = self.context.rpc();
        send_recv!(rpc, StreamStart(self.token) => StreamStarted)
    }

    fn stop(&mut self) -> Result<()> {
        assert_not_in_callback();
        let rpc = self.context.rpc();
        send_recv!(rpc, StreamStop(self.token) => StreamStopped)
    }

    fn position(&mut self) -> Result<u64> {
        assert_not_in_callback();
        let mut calls = self.cached_calls;
        calls.1 += 1;
        if let Some((last_pos, last_time)) = self.cached_position {
            // TODO: add tuneable for 10ms cache lifetime.
            if last_time.elapsed() < Duration::from_millis(10) {
                calls.0 += 1;
                // TODO: Needs to be capped by written_pos from data_cb.
                // TODO: Need to avoid returning < this estimate after any uncached call.
                let current_pos = last_pos as u128
                    + (last_time.elapsed().as_millis() * self.stream_output_rate.unwrap() as u128
                        / 1000);
                self.cached_calls = calls;
                return Ok(current_pos.try_into().unwrap());
            }
        }
        let rpc = self.context.rpc();
        let current_pos = send_recv!(rpc, StreamGetPosition(self.token) => StreamPosition())?;
        // TODO: server should send timestamp.
        self.cached_position = Some((current_pos, Instant::now()));
        // TODO: Ensure this is never < a value returned via the cached estimate path.
        self.cached_calls = calls;
        Ok(current_pos)
    }

    fn latency(&mut self) -> Result<u32> {
        assert_not_in_callback();
        let rpc = self.context.rpc();
        send_recv!(rpc, StreamGetLatency(self.token) => StreamLatency())
    }

    fn input_latency(&mut self) -> Result<u32> {
        assert_not_in_callback();
        let rpc = self.context.rpc();
        send_recv!(rpc, StreamGetInputLatency(self.token) => StreamInputLatency())
    }

    fn set_volume(&mut self, volume: f32) -> Result<()> {
        assert_not_in_callback();
        let rpc = self.context.rpc();
        send_recv!(rpc, StreamSetVolume(self.token, volume) => StreamVolumeSet)
    }

    fn set_name(&mut self, name: &CStr) -> Result<()> {
        assert_not_in_callback();
        let rpc = self.context.rpc();
        send_recv!(rpc, StreamSetName(self.token, name.to_owned()) => StreamNameSet)
    }

    fn current_device(&mut self) -> Result<&DeviceRef> {
        assert_not_in_callback();
        let rpc = self.context.rpc();
        match send_recv!(rpc, StreamGetCurrentDevice(self.token) => StreamCurrentDevice()) {
            Ok(d) => Ok(unsafe { DeviceRef::from_ptr(Box::into_raw(Box::new(d.into()))) }),
            Err(e) => Err(e),
        }
    }

    fn device_destroy(&mut self, device: &DeviceRef) -> Result<()> {
        assert_not_in_callback();
        // It's all unsafe...
        if device.as_ptr().is_null() {
            Err(Error::error())
        } else {
            unsafe {
                let _: Box<Device> = Box::from_raw(device.as_ptr() as *mut _);
            }
            Ok(())
        }
    }

    fn register_device_changed_callback(
        &mut self,
        device_changed_callback: ffi::cubeb_device_changed_callback,
    ) -> Result<()> {
        assert_not_in_callback();
        let rpc = self.context.rpc();
        let enable = device_changed_callback.is_some();
        *self.device_change_cb.lock().unwrap() = device_changed_callback;
        send_recv!(rpc, StreamRegisterDeviceChangeCallback(self.token, enable) => StreamRegisterDeviceChangeCallback)
    }
}

pub fn init(
    ctx: &ClientContext,
    init_params: messages::StreamInitParams,
    data_callback: ffi::cubeb_data_callback,
    state_callback: ffi::cubeb_state_callback,
    user_ptr: *mut c_void,
) -> Result<Stream> {
    let stm = ClientStream::init(ctx, init_params, data_callback, state_callback, user_ptr)?;
    debug_assert_eq!(stm.user_ptr(), user_ptr);
    Ok(stm)
}
