use cinemotion_core::{AttributeValue, Scene, SceneAttribute, SceneObject};
use cinemotion_protocol::Mode;
use cinemotion_server::{ServerConfig, ServerHandle};
use pyo3::{exceptions::PyRuntimeError, prelude::*};

#[pyclass(name = "CineServer")]
pub struct PyCineServer {
    server: ServerHandle,
    rt: tokio::runtime::Runtime,
}

#[pymethods]
impl PyCineServer {
    #[new]
    #[pyo3(signature = (name=None))]
    pub fn new(name: Option<String>) -> PyResult<Self> {
        let mut config = ServerConfig::default();
        if let Some(name) = name {
            config.name = name;
        }
        config.quic_bind_addr = "127.0.0.1:0".parse().expect("static address parses");
        config.enable_discovery = false;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;

        Ok(Self {
            server: ServerHandle::new(config),
            rt,
        })
    }

    pub fn start(&self) -> PyResult<String> {
        let adv = self
            .rt
            .block_on(self.server.start())
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        Ok(format!("{}:{}", adv.bind_host, adv.bind_port))
    }

    pub fn stop(&self) -> PyResult<()> {
        self.rt
            .block_on(self.server.stop())
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))
    }

    pub fn create_default_scene(&self, name: String) -> PyResult<String> {
        let object = SceneObject::new("camera").with_attribute(SceneAttribute::new(
            "position",
            AttributeValue::Vec3f([0.0, 0.0, 0.0]),
        ));
        let scene = Scene::new(name).with_object(object);
        let scene_id = self.rt.block_on(self.server.load_scene(scene));
        Ok(scene_id.to_string())
    }

    pub fn set_live_mode(&self) -> PyResult<()> {
        self.rt
            .block_on(self.server.set_mode(Mode::Live))
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))
    }

    pub fn metrics(&self) -> PyResult<(u64, u64, u64, u64, u64, u64, u64)> {
        let metrics = self.rt.block_on(self.server.metrics());
        Ok((
            metrics.accepted_sessions,
            metrics.rejected_sessions,
            metrics.motion_datagrams,
            metrics.motion_updates,
            metrics.signaling_messages,
            metrics.scheduler_ticks,
            metrics.publish_ticks,
        ))
    }
}

#[pymodule]
fn cinemotion_sdk_rust(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyCineServer>()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::PyCineServer;

    #[test]
    fn rust_binding_constructs_server() {
        let server = PyCineServer::new(Some("py-test".into())).expect("py server should build");
        let _ = server.start().expect("start should succeed");
        server.stop().expect("stop should succeed");
    }
}
