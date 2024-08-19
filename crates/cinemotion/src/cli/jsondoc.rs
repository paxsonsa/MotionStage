use anyhow::Result;
use cinemotion_proto as protocol;
use clap::Args;
use std::io::Write;
use termcolor::{Color, ColorChoice, ColorSpec, StandardStream, WriteColor};

#[derive(Args)]
pub struct JsonDocCmd {}

impl JsonDocCmd {
    pub async fn run(&self) -> Result<i32> {
        let mut stdout = StandardStream::stdout(ColorChoice::Always);
        stdout.set_color(ColorSpec::new().set_bold(true))?;
        writeln!(&mut stdout, "\nClient Messages\n-----------------------")?;

        self.print_client_message(
            "Ping",
            protocol::ClientMessage {
                body: Some(protocol::client_message::Body::Ping(protocol::Ping {})),
            },
            &mut stdout,
        )?;

        self.print_client_message(
            "Pong",
            protocol::ClientMessage {
                body: Some(protocol::client_message::Body::Pong(protocol::Pong {})),
            },
            &mut stdout,
        )?;

        self.print_client_message(
            "DeviceInitAck",
            protocol::ClientMessage {
                body: Some(protocol::client_message::Body::DeviceInitAck(
                    protocol::DeviceInitAck {
                        device_spec: Some(protocol::DeviceSpec {
                            name: "device_name".to_string(),
                            attributes: [(
                                "transform".to_string(),
                                protocol::AttributeValue {
                                    value: Some(protocol::attribute_value::Value::Vec4(
                                        protocol::Vec4 {
                                            x: 10.0,
                                            y: 20.0,
                                            z: 30.0,
                                            w: 40.0,
                                        },
                                    )),
                                },
                            )]
                            .into(),
                        }),
                    },
                )),
            },
            &mut stdout,
        )?;

        self.print_client_message(
            "DeviceSample",
            protocol::ClientMessage {
                body: Some(protocol::client_message::Body::DeviceSample(
                    protocol::DeviceSample {
                        attributes: [
                            (
                                "vec4_attr".to_string(),
                                protocol::AttributeValue {
                                    value: Some(protocol::attribute_value::Value::Vec4(
                                        protocol::Vec4 {
                                            x: 1.0,
                                            y: 2.0,
                                            z: 3.0,
                                            w: 4.0,
                                        },
                                    )),
                                },
                            ),
                            (
                                "vec3_attr".to_string(),
                                protocol::AttributeValue {
                                    value: Some(protocol::attribute_value::Value::Vec3(
                                        protocol::Vec3 {
                                            x: 1.0,
                                            y: 2.0,
                                            z: 3.0,
                                        },
                                    )),
                                },
                            ),
                            (
                                "matrix_attr".to_string(),
                                protocol::AttributeValue {
                                    value: Some(protocol::attribute_value::Value::Matrix44(
                                        protocol::Matrix44 {
                                            values: vec![
                                                1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0,
                                                11.0, 12.0, 13.0, 14.0, 15.0, 16.0,
                                            ],
                                        },
                                    )),
                                },
                            ),
                        ]
                        .into(),
                    },
                )),
            },
            &mut stdout,
        )?;

        self.print_client_message(
            "SceneCreateObject",
            protocol::ClientMessage {
                body: Some(protocol::client_message::Body::SceneCreateObject(
                    protocol::SceneCreateObject {
                        spec: Some(protocol::SceneObjectSpec {
                            name: "test".to_string(),
                            links: vec![protocol::AttributeLink {
                                device_id: 10,
                                device_attr: "transform".to_string(),
                                attribute: "position".to_string(),
                            }],
                            attributes: vec![protocol::Attribute {
                                name: "position".to_string(),
                                default_value: Some(protocol::AttributeValue {
                                    value: Some(protocol::attribute_value::Value::Vec4(
                                        protocol::Vec4 {
                                            x: 1.0,
                                            y: 2.0,
                                            z: 3.0,
                                            w: 4.0,
                                        },
                                    )),
                                }),
                                value: None,
                            }],
                        }),
                    },
                )),
            },
            &mut stdout,
        )?;

        self.print_client_message(
            "SceneUpdateObject",
            protocol::ClientMessage {
                body: Some(protocol::client_message::Body::SceneUpdateObject(
                    protocol::SceneUpdateObject {
                        id: 10,
                        spec: Some(protocol::SceneObjectSpec {
                            name: "objectA".to_string(),
                            attributes: vec![protocol::Attribute {
                                name: "position".to_string(),
                                default_value: Some(protocol::AttributeValue {
                                    value: Some(protocol::attribute_value::Value::Vec4(
                                        protocol::Vec4 {
                                            x: 1.0,
                                            y: 2.0,
                                            z: 3.0,
                                            w: 4.0,
                                        },
                                    )),
                                }),
                                value: None,
                            }],
                            links: vec![protocol::AttributeLink {
                                device_id: 10,
                                device_attr: "transform".to_string(),
                                attribute: "position".to_string(),
                            }],
                        }),
                    },
                )),
            },
            &mut stdout,
        )?;

        self.print_client_message(
            "SceneDeleteObject",
            protocol::ClientMessage {
                body: Some(protocol::client_message::Body::SceneDeleteObject(
                    protocol::SceneDeleteObject { id: 10 },
                )),
            },
            &mut stdout,
        )?;

        self.print_client_message(
            "MotionSetMode",
            protocol::ClientMessage {
                body: Some(protocol::client_message::Body::MotionSetMode(
                    protocol::MotionSetMode { enabled: true },
                )),
            },
            &mut stdout,
        )?;

        println!("");
        Ok(0)
    }

    fn print_client_message<M>(
        &self,
        name: &str,
        message: M,
        writer: &mut StandardStream,
    ) -> Result<()>
    where
        M: serde::Serialize,
    {
        writer.set_color(ColorSpec::new().set_fg(Some(Color::Yellow)).set_bold(true))?;
        writeln!(writer, "\n{}", name)?;
        writer.reset()?;
        serde_json::to_writer_pretty(&mut *writer, &message)?;
        writer.write_all(b"\n")?;
        writer.reset()?;
        Ok(())
    }
}
