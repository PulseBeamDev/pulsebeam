use pulsebeam_agent_core::{
    E2eeDirection, E2eeDomain, E2eeEncryptor, E2eeEpoch, E2eeError, E2eeKeyRing, E2eeMasterKey,
    E2eeReceiver,
};

#[cfg(target_arch = "wasm32")]
use crate::interop::WebError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransformDirection {
    Encrypt,
    Decrypt,
}

struct E2eeState {
    sender: E2eeEncryptor,
    receiver: E2eeReceiver,
}

pub struct E2eeContext {
    state: std::rc::Rc<std::cell::RefCell<E2eeState>>,
}

impl E2eeContext {
    pub fn new(
        key: E2eeMasterKey,
        epoch: E2eeEpoch,
        sender: impl Into<String>,
        stream: impl Into<String>,
    ) -> Result<Self, E2eeError> {
        let key_id = key.key_id;
        let sender = sender.into();
        let stream = stream.into();
        let send_domain = E2eeDomain::new(&sender, &stream, E2eeDirection::Send)?;
        let mut ring = E2eeKeyRing::new(2)?;
        ring.install(key, epoch, send_domain.clone())?;
        Ok(Self {
            state: std::rc::Rc::new(std::cell::RefCell::new(E2eeState {
                sender: ring.encryptor(key_id, epoch, &send_domain)?,
                receiver: ring.receiver(key_id, epoch, &send_domain)?,
            })),
        })
    }

    pub fn encrypt_frame(&mut self, frame: &[u8]) -> Result<Vec<u8>, E2eeError> {
        self.state.borrow_mut().sender.encrypt(frame)
    }

    pub fn decrypt_frame(&mut self, frame: &[u8]) -> Result<Vec<u8>, E2eeError> {
        self.state.borrow_mut().receiver.decrypt(frame)
    }

    #[cfg(target_arch = "wasm32")]
    pub fn install_on(
        &self,
        rtp: &wasm_bindgen::JsValue,
        direction: TransformDirection,
    ) -> Result<EncodedTransform, WebError> {
        EncodedTransform::install(rtp, direction, self.state.clone())
    }
}

#[cfg(target_arch = "wasm32")]
pub struct EncodedTransform {
    worker: web_sys::Worker,
    port: web_sys::MessagePort,
    object_url: String,
    _callback: wasm_bindgen::closure::Closure<dyn FnMut(web_sys::MessageEvent)>,
}

#[cfg(target_arch = "wasm32")]
impl EncodedTransform {
    fn install(
        rtp: &wasm_bindgen::JsValue,
        direction: TransformDirection,
        context: std::rc::Rc<std::cell::RefCell<E2eeState>>,
    ) -> Result<Self, WebError> {
        use js_sys::{Array, Object, Reflect};
        use wasm_bindgen::{JsCast, JsValue, closure::Closure};

        let source = Array::new();
        source.push(&JsValue::from_str(worker_source()));
        let blob = web_sys::Blob::new_with_str_sequence(&source).map_err(js_error)?;
        let object_url = web_sys::Url::create_object_url_with_blob(&blob).map_err(js_error)?;
        let worker = web_sys::Worker::new(&object_url).map_err(js_error)?;
        let channel = web_sys::MessageChannel::new().map_err(js_error)?;
        let port = channel.port1();
        let reply_port = port.clone();
        let callback = Closure::wrap(Box::new(move |event: web_sys::MessageEvent| {
            let data = event.data();
            let id = Reflect::get(&data, &JsValue::from_str("id"))
                .ok()
                .and_then(|value| value.as_f64())
                .unwrap_or(0.0);
            let input = Reflect::get(&data, &JsValue::from_str("data"))
                .ok()
                .map(|value| js_sys::Uint8Array::new(&value).to_vec())
                .unwrap_or_default();
            let direction = direction;
            let output = context
                .try_borrow_mut()
                .ok()
                .and_then(|mut context| match direction {
                    TransformDirection::Encrypt => context.sender.encrypt(&input).ok(),
                    TransformDirection::Decrypt => context.receiver.decrypt(&input).ok(),
                });
            let message = Object::new();
            let _ = Reflect::set(&message, &JsValue::from_str("id"), &id.into());
            if let Some(output) = output {
                let bytes = js_sys::Uint8Array::from(output.as_slice());
                let _ = Reflect::set(&message, &JsValue::from_str("data"), &bytes);
            } else {
                let _ = Reflect::set(&message, &JsValue::from_str("error"), &true.into());
            }
            let _ = reply_port.post_message(&message);
        }) as Box<dyn FnMut(web_sys::MessageEvent)>);
        port.set_onmessage(Some(callback.as_ref().unchecked_ref()));
        port.start();

        let options = Object::new();
        Reflect::set(
            &options,
            &JsValue::from_str("operation"),
            &JsValue::from_str(match direction {
                TransformDirection::Encrypt => "encrypt",
                TransformDirection::Decrypt => "decrypt",
            }),
        )
        .map_err(js_error)?;
        Reflect::set(&options, &JsValue::from_str("port"), &channel.port2()).map_err(js_error)?;
        let constructor = Reflect::get(
            &js_sys::global(),
            &JsValue::from_str("RTCRtpScriptTransform"),
        )
        .map_err(js_error)?
        .dyn_into::<js_sys::Function>()
        .map_err(|_| WebError::E2ee("RTCRtpScriptTransform unavailable".to_owned()))?;
        let transform =
            Reflect::construct(&constructor, &Array::of2(&worker, &options)).map_err(js_error)?;
        Reflect::set(rtp, &JsValue::from_str("transform"), &transform).map_err(js_error)?;
        Ok(Self {
            worker,
            port,
            object_url,
            _callback: callback,
        })
    }
}

#[cfg(target_arch = "wasm32")]
impl Drop for EncodedTransform {
    fn drop(&mut self) {
        self.worker.terminate();
        let _ = web_sys::Url::revoke_object_url(&self.object_url);
        self.port.close();
    }
}

#[cfg(target_arch = "wasm32")]
fn worker_source() -> &'static str {
    r#"self.onrtctransform = event => {
  const transformer = event.transformer;
  const port = transformer.options.port;
  const pending = new Map();
  let nextId = 0;
  port.onmessage = message => {
    const request = pending.get(message.data.id);
    if (!request) return;
    pending.delete(message.data.id);
    clearTimeout(request.timer);
    if (message.data.error) request.reject(new Error("frame transform failed"));
    else request.resolve(message.data.data);
  };
  port.start();
  const process = data => new Promise((resolve, reject) => {
    const id = nextId++;
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error("frame transform timed out"));
    }, 5000);
    pending.set(id, { resolve, reject, timer });
    port.postMessage({ id, data });
  });
  transformer.readable.pipeThrough(new TransformStream({
    async transform(frame, controller) {
      const data = await process(frame.data);
      frame.data = data;
      controller.enqueue(frame);
    }
  })).pipeTo(transformer.writable);
};"#
}

#[cfg(target_arch = "wasm32")]
fn js_error(error: wasm_bindgen::JsValue) -> WebError {
    WebError::E2ee(
        error
            .as_string()
            .unwrap_or_else(|| "encoded transform failure".to_owned()),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn rust_e2ee_context_owns_frame_crypto() {
        let key = E2eeMasterKey::new(9, [7; 32]);
        let epoch = E2eeEpoch::new([3; 16]).unwrap();
        let mut sender = E2eeContext::new(key.clone(), epoch, "sender", "stream").unwrap();
        let mut receiver = E2eeContext::new(key, epoch, "sender", "stream").unwrap();
        let frame = sender.encrypt_frame(b"frame").unwrap();
        assert_eq!(receiver.decrypt_frame(&frame).unwrap(), b"frame");
    }
}
