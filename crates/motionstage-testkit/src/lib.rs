use motionstage_protocol::{ClientHello, ClientRole, Feature, RegisterRequest};
use motionstage_server::{ServerConfig, ServerError, ServerHandle, ServerMetrics};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub struct TestHarness {
    pub server: ServerHandle,
}

impl TestHarness {
    pub fn new() -> Self {
        Self {
            server: ServerHandle::new(ServerConfig::default()),
        }
    }

    pub async fn bootstrap_motion_client(&self, name: &str) -> Result<Uuid, ServerError> {
        let device_id = Uuid::now_v7();

        self.server.discovered(device_id, name).await?;
        self.server.transport_connected(device_id).await?;
        self.server
            .hello_exchanged(ClientHello {
                protocol_major: motionstage_protocol::PROTOCOL_MAJOR,
                protocol_minor: motionstage_protocol::PROTOCOL_MINOR,
                device_id,
                device_name: name.into(),
                roles: vec![ClientRole::MotionSource],
                features: vec![Feature::Motion],
                advertised_attributes: vec!["pose_pos".into()],
            })
            .await?;
        self.server.authenticate(device_id).await?;
        self.server
            .register(
                device_id,
                RegisterRequest {
                    pairing_token: None,
                    api_key: None,
                },
            )
            .await?;
        self.server.scene_synced(device_id).await?;
        self.server.activate(device_id).await?;

        Ok(device_id)
    }

    pub async fn run_motion_soak(
        &self,
        device_id: Uuid,
        duration: Duration,
        input_hz: u32,
        output_attribute: &str,
    ) -> Result<SoakReport, ServerError> {
        let interval_ns = (1_000_000_000_u64 / input_hz.max(1) as u64).max(1);
        let started = Instant::now();
        let mut sent = 0_u64;
        let mut t_ns = 0_u64;

        while started.elapsed() < duration {
            self.server
                .ingest_motion_samples(
                    device_id,
                    vec![motionstage_core::AttributeUpdate {
                        output_attribute: output_attribute.to_owned(),
                        value: motionstage_core::AttributeValue::Vec3f([
                            sent as f32,
                            sent as f32,
                            sent as f32,
                        ]),
                    }],
                    t_ns,
                )
                .await?;
            sent += 1;
            t_ns = t_ns.saturating_add(interval_ns);
            tokio::time::sleep(Duration::from_nanos(interval_ns)).await;
        }

        Ok(SoakReport {
            sent_samples: sent,
            tick_count: self.server.tick_count().await,
            metrics: self.server.metrics().await,
        })
    }
}

impl Default for TestHarness {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct SoakReport {
    pub sent_samples: u64,
    pub tick_count: u64,
    pub metrics: ServerMetrics,
}

#[cfg(test)]
mod tests {
    use motionstage_core::{AttributeValue, MappingRequest, Scene, SceneAttribute, SceneObject};
    use motionstage_protocol::Mode;
    use motionstage_protocol::SessionState;
    use std::time::Duration;

    use super::TestHarness;

    #[tokio::test]
    async fn bootstrap_client_reaches_active_state() {
        let harness = TestHarness::new();
        let device_id = harness.bootstrap_motion_client("ipad").await.unwrap();
        let session = harness.server.session_info(device_id).await.unwrap();
        assert_eq!(session.state, SessionState::Active);
    }

    #[tokio::test]
    async fn soak_test_hits_motion_pipeline() {
        let harness = TestHarness::new();
        let device_id = harness.bootstrap_motion_client("ipad").await.unwrap();

        let object = SceneObject::new("camera").with_attribute(SceneAttribute::new(
            "position",
            AttributeValue::Vec3f([0.0, 0.0, 0.0]),
        ));
        let object_id = object.id;
        let scene = Scene::new("soak").with_object(object);
        let scene_id = scene.id;
        harness.server.load_scene(scene).await;
        harness
            .server
            .create_mapping(
                MappingRequest {
                    source_device: device_id,
                    source_output: "pose_pos".into(),
                    target_scene: scene_id,
                    target_object: object_id,
                    target_attribute: "position".into(),
                    component_mask: None,
                },
                0,
            )
            .await
            .unwrap();
        harness.server.set_mode(Mode::Live).await.unwrap();

        let report = harness
            .run_motion_soak(device_id, Duration::from_millis(120), 60, "pose_pos")
            .await
            .unwrap();

        assert!(report.sent_samples > 0);
        assert!(report.tick_count > 0);
        assert!(report.metrics.motion_updates >= report.sent_samples);
    }

    #[tokio::test]
    async fn soak_test_reaches_120hz_ingest_target_window() {
        let harness = TestHarness::new();
        let device_id = harness.bootstrap_motion_client("ipad").await.unwrap();

        let object = SceneObject::new("camera").with_attribute(SceneAttribute::new(
            "position",
            AttributeValue::Vec3f([0.0, 0.0, 0.0]),
        ));
        let object_id = object.id;
        let scene = Scene::new("soak120").with_object(object);
        let scene_id = scene.id;
        harness.server.load_scene(scene).await;
        harness
            .server
            .create_mapping(
                MappingRequest {
                    source_device: device_id,
                    source_output: "pose_pos".into(),
                    target_scene: scene_id,
                    target_object: object_id,
                    target_attribute: "position".into(),
                    component_mask: None,
                },
                0,
            )
            .await
            .unwrap();
        harness.server.set_mode(Mode::Live).await.unwrap();

        let report = harness
            .run_motion_soak(device_id, Duration::from_millis(250), 120, "pose_pos")
            .await
            .unwrap();

        assert!(report.sent_samples >= 20);
        assert!(report.metrics.motion_updates >= report.sent_samples);
        assert!(report.tick_count >= report.sent_samples);
    }

    #[tokio::test]
    async fn multi_client_soak_accumulates_motion_updates() {
        let harness = TestHarness::new();
        let device_a = harness.bootstrap_motion_client("ipad-a").await.unwrap();
        let device_b = harness.bootstrap_motion_client("ipad-b").await.unwrap();

        let object_a = SceneObject::new("camera_a").with_attribute(SceneAttribute::new(
            "position",
            AttributeValue::Vec3f([0.0, 0.0, 0.0]),
        ));
        let object_b = SceneObject::new("camera_b").with_attribute(SceneAttribute::new(
            "position",
            AttributeValue::Vec3f([0.0, 0.0, 0.0]),
        ));
        let object_a_id = object_a.id;
        let object_b_id = object_b.id;
        let scene = Scene::new("multi")
            .with_object(object_a)
            .with_object(object_b);
        let scene_id = scene.id;
        harness.server.load_scene(scene).await;

        harness
            .server
            .create_mapping(
                MappingRequest {
                    source_device: device_a,
                    source_output: "pose_a".into(),
                    target_scene: scene_id,
                    target_object: object_a_id,
                    target_attribute: "position".into(),
                    component_mask: None,
                },
                0,
            )
            .await
            .unwrap();
        harness
            .server
            .create_mapping(
                MappingRequest {
                    source_device: device_b,
                    source_output: "pose_b".into(),
                    target_scene: scene_id,
                    target_object: object_b_id,
                    target_attribute: "position".into(),
                    component_mask: None,
                },
                0,
            )
            .await
            .unwrap();
        harness.server.set_mode(Mode::Live).await.unwrap();

        let (a_report, b_report) = tokio::join!(
            harness.run_motion_soak(device_a, Duration::from_millis(120), 60, "pose_a"),
            harness.run_motion_soak(device_b, Duration::from_millis(120), 60, "pose_b"),
        );
        let a_report = a_report.unwrap();
        let b_report = b_report.unwrap();
        let metrics = harness.server.metrics().await;
        assert!(metrics.motion_updates >= a_report.sent_samples + b_report.sent_samples);
    }

    #[tokio::test]
    async fn scheduler_counters_progress_at_runtime_start() {
        let mut config = motionstage_server::ServerConfig::default();
        config.enable_discovery = false;
        config.quic_bind_addr = "127.0.0.1:0".parse().unwrap();
        let server = motionstage_server::ServerHandle::new(config);

        server.start().await.unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
        let metrics = server.metrics().await;
        assert!(metrics.scheduler_ticks > 0);
        assert!(metrics.publish_ticks > 0);
        server.stop().await.unwrap();
    }
}
