/*
 * Copyright 2020-present, Nike, Inc.
 * All rights reserved.
 *
 * This source code is licensed under the Apache-2.0 license found in
 * the LICENSE file in the root of this source tree.
 */

use crate::bindings::*;
use crate::error::GGError;
use crate::handler::{Handler, LambdaContext};
use crate::GGResult;
use crossbeam_channel::{unbounded, Receiver, Sender};
use lazy_static::lazy_static;
use log::{error, info};
use std::default::Default;
use std::ffi::CStr;
use std::os::raw::c_void;
use std::sync::Arc;
use std::thread;

/// The size of the buffer for reading content received via the C SDK
const BUFFER_SIZE: usize = 100;

/// Denotes a handler that is thread safe
pub type ShareableHandler = dyn Handler + Send + Sync;

lazy_static! {
    // This establishes a thread safe global channel that can
    // be acquired from the callback function we register with the C Api
    static ref CHANNEL: Arc<ChannelHolder> = ChannelHolder::new();
}

/// Type of runtime. Currently only one, Async exits
pub enum RuntimeOption {
    /// The runtime will be started in the current thread an block preventing exit.
    /// This is the option that should be used for On-demand greengrass lambda functions.
    /// This is the default option.
    Sync,
    /// The runtime will start in a new thread. If main() exits, the runtime stops.
    /// This is useful for long lived lambda functions.
    Async,
}

impl RuntimeOption {
    /// Converts to the option required by the runtime api
    fn as_opt(&self) -> gg_runtime_opt {
        match self {
            Self::Sync => 0, // for some reason they don't spell out this option in the header
            Self::Async => gg_runtime_opt_GG_RT_OPT_ASYNC,
        }
    }
}

/// Configures and instantiates the green grass core runtime
/// Runtime can only be started by the Initializer. You must pass the runtime into the [`Initializer::with_runtime`] method.
pub struct Runtime {
    runtime_option: RuntimeOption,
    handler: Option<Box<ShareableHandler>>,
}

impl Default for Runtime {
    fn default() -> Self {
        Runtime {
            runtime_option: RuntimeOption::Sync,
            handler: None,
        }
    }
}

impl Runtime {
    /// Start the green grass core runtime
    pub(crate) fn start(self) -> GGResult<()> {
        unsafe {
            // If there is a handler defined, then register the
            // the c delegating handler and start a thread that
            // monitors the channel for messages from the c handler
            let c_handler = if let Some(handler) = self.handler {
                thread::spawn(move || loop {
                    match ChannelHolder::recv() {
                        Ok(context) => handler.handle(context),
                        Err(e) => error!("{}", e),
                    }
                });

                delgating_handler
            } else {
                no_op_handler
            };

            let start_res = gg_runtime_start(Some(c_handler), self.runtime_option.as_opt());
            GGError::from_code(start_res)?;
        }
        Ok(())
    }

    /// Provide a non-default runtime option
    pub fn with_runtime_option(self, runtime_option: RuntimeOption) -> Self {
        Runtime {
            runtime_option,
            ..self
        }
    }

    /// Provide a handler. If no handler is provided the runtime will register a no-op handler
    ///
    /// ```rust
    /// use aws_greengrass_core_rust::handler::{Handler, LambdaContext};
    /// use aws_greengrass_core_rust::runtime::Runtime;
    ///
    /// struct MyHandler;
    ///
    /// impl Handler for MyHandler {
    ///     fn handle(&self, ctx: LambdaContext) {
    ///         // Do something here
    ///     }
    /// }
    ///
    /// Runtime::default().with_handler(Some(Box::new(MyHandler)));
    /// ```
    pub fn with_handler(self, handler: Option<Box<ShareableHandler>>) -> Self {
        Runtime { handler, ..self }
    }
}

/// c handler that performs a no op
extern "C" fn no_op_handler(_: *const gg_lambda_context) {
    info!("No opt handler called!");
}

/// c handler that utilizes ChannelHandler in order to pass
/// information to the Handler implementation provided
extern "C" fn delgating_handler(c_ctx: *const gg_lambda_context) {
    info!("delegating_handler called!");
    unsafe {
        let result = build_context(c_ctx).and_then(ChannelHolder::send);
        if let Err(e) = result {
            error!("{}", e);
        }
    }
}

/// Converts the c context to our rust native context
unsafe fn build_context(c_ctx: *const gg_lambda_context) -> GGResult<LambdaContext> {
    let message = handler_read_message()?;
    let function_arn = CStr::from_ptr((*c_ctx).function_arn)
        .to_string_lossy()
        .to_owned()
        .to_string();
    let client_context = CStr::from_ptr((*c_ctx).client_context)
        .to_string_lossy()
        .to_owned()
        .to_string();
    Ok(LambdaContext::new(function_arn, client_context, message))
}

/// Wraps the C gg_lambda_handler_read call
unsafe fn handler_read_message() -> GGResult<Vec<u8>> {
    let mut collected: Vec<u8> = Vec::new();
    loop {
        let mut buffer = [0u8; BUFFER_SIZE];
        let mut read: usize = 0;

        let raw_read = &mut read as *mut usize;

        let pub_res =
            gg_lambda_handler_read(buffer.as_mut_ptr() as *mut c_void, BUFFER_SIZE, raw_read);

        GGError::from_code(pub_res)?;

        if read > 0 {
            collected.extend_from_slice(&buffer[..read]);
        } else {
            break;
        }
    }
    Ok(collected)
}

/// Wraps a Channel.
/// This is mostly needed as there is no way to instantiate a static ref with a tuple (see CHANNEL above)
struct ChannelHolder {
    sender: Sender<LambdaContext>,
    receiver: Receiver<LambdaContext>,
}

impl ChannelHolder {
    pub fn new() -> Arc<Self> {
        let (sender, receiver) = unbounded();
        let holder = ChannelHolder { sender, receiver };
        Arc::new(holder)
    }

    /// Performs a send with CHANNEL and coerces the error type
    fn send(context: LambdaContext) -> GGResult<()> {
        Arc::clone(&CHANNEL)
            .sender
            .send(context)
            .map_err(GGError::from)
    }

    /// Performs a recv with CHANNEL and coerces the error type
    fn recv() -> GGResult<LambdaContext> {
        Arc::clone(&CHANNEL).receiver.recv().map_err(GGError::from)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::handler::{Handler, LambdaContext};
    use crate::Initializer;
    use crossbeam_channel::{bounded, Sender};
    use std::ffi::CString;
    use std::time::Duration;

    #[test]
    fn test_build_context() {
        unsafe {
            let my_message = b"My handlers message";
            GG_LAMBDA_HANDLER_READ_BUFFER.with(|b| b.replace(my_message.to_owned().to_vec()));

            let my_function_arn = "this is a function arn";
            let client_ctx = "this is my client context";

            let my_function_arn_c = CString::new(my_function_arn).unwrap();
            let client_ctx_c = CString::new(client_ctx).unwrap();

            let lambda_context_c = Box::new(gg_lambda_context {
                function_arn: my_function_arn_c.as_ptr(),
                client_context: client_ctx_c.as_ptr(),
            });

            let raw_ctx = Box::into_raw(lambda_context_c);
            let context_result = build_context(raw_ctx);
            let _ = Box::from_raw(raw_ctx);
            let context = context_result.expect("Building a context should be successful");

            assert_eq!(context.function_arn, my_function_arn);
            assert_eq!(context.client_context, client_ctx);
            assert_eq!(context.message, my_message);
        }
    }

    #[derive(Clone)]
    struct TestHandler {
        sender: Sender<LambdaContext>,
    }

    impl TestHandler {
        fn new(sender: Sender<LambdaContext>) -> Self {
            TestHandler { sender }
        }
    }

    impl Handler for TestHandler {
        fn handle(&self, ctx: LambdaContext) {
            self.sender.send(ctx).expect("Could not send context");
        }
    }

    #[cfg(not(feature = "mock"))]
    #[test]
    fn test_handler() {
        reset_test_state();
        let (sender, receiver) = bounded(1);
        let handler = TestHandler::new(sender);
        let runtime = Runtime::default()
            .with_runtime_option(RuntimeOption::Sync)
            .with_handler(Some(Box::new(handler.clone())));
        Initializer::default()
            .with_runtime(runtime)
            .init()
            .expect("Initialization failed");
        let context = LambdaContext::new(
            "my_function_arn".to_owned(),
            "my_context".to_owned(),
            b"my bytes".to_ascii_lowercase(),
        );
        send_to_handler(context.clone());
        // a long time out in order to ensure that it will succeed when testing with coverage
        let ctx = receiver
            .recv_timeout(Duration::from_secs(120))
            .expect("Context was sent within the timeout period");
        assert_eq!(ctx, context);
    }
}
