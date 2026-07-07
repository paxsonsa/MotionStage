import XCTest
@testable import MotionStageClient

final class MotionStageClientTests: XCTestCase {
    func testRuntimeModeConstantsRemainStable() {
        XCTAssertEqual(RuntimeMode.idle, RuntimeMode(dataFlow: .idle, recording: .inactive))
        XCTAssertEqual(RuntimeMode.live, RuntimeMode(dataFlow: .live, recording: .inactive))
        XCTAssertEqual(RuntimeMode.recording, RuntimeMode(dataFlow: .live, recording: .recording))
        XCTAssertEqual(RuntimeMode.playback, RuntimeMode(dataFlow: .live, recording: .playback))
    }

    func testDecodeModeChangedStateEventMessage() throws {
        let json = """
        {"kind":"state_event","seq":7,
         "origin_session":"018f5ca9-e8f4-7fd3-a923-4b7a25a6f4df",
         "timestamp_ns":123,
         "event":{"type":"ModeChanged","data":{"mode":{"data_flow":"Live","recording":"Inactive"}}}}
        """
        let update = try XCTUnwrap(StateEventUpdate(streamJSON: json))
        guard case .event(let envelope) = update else {
            return XCTFail("expected .event, got \(update)")
        }
        XCTAssertEqual(envelope.seq, 7)
        XCTAssertEqual(
            envelope.originSession,
            UUID(uuidString: "018F5CA9-E8F4-7FD3-A923-4B7A25A6F4DF")
        )
        XCTAssertEqual(envelope.timestampNs, 123)
        XCTAssertEqual(envelope.event, .modeChanged(mode: .live))
    }

    func testDecodeMappingCreatedStateEventMessage() throws {
        let json = """
        {"kind":"state_event","seq":9,"origin_session":null,"timestamp_ns":5,
         "event":{"type":"MappingCreated","data":{"mapping":{
            "mapping_id":"018f5ca9-e8f4-7fd3-a923-4b7a25a6f4df",
            "source_device":"018f5ca9-e8f4-7fd3-a923-4b7a25a6f4d0",
            "source_output":"camera.position",
            "target_scene":"018f5ca9-e8f4-7fd3-a923-4b7a25a6f4d1",
            "target_object":"018f5ca9-e8f4-7fd3-a923-4b7a25a6f4d2",
            "target_attribute":"position",
            "component_mask":[0,2],
            "lock":false}}}}
        """
        let update = try XCTUnwrap(StateEventUpdate(streamJSON: json))
        guard case .event(let envelope) = update,
              case .mappingCreated(let mapping) = envelope.event
        else {
            return XCTFail("expected MappingCreated event")
        }
        XCTAssertNil(envelope.originSession)
        XCTAssertEqual(mapping.sourceOutput, "camera.position")
        XCTAssertEqual(mapping.componentMask, [0, 2])
        XCTAssertFalse(mapping.lock)
    }

    func testUnknownStateEventVariantDecodesAsUnknown() throws {
        let json = """
        {"kind":"state_event","seq":1,"origin_session":null,"timestamp_ns":1,
         "event":{"type":"SomeFutureEvent","data":{"whatever":true}}}
        """
        let update = try XCTUnwrap(StateEventUpdate(streamJSON: json))
        guard case .event(let envelope) = update else {
            return XCTFail("expected .event")
        }
        XCTAssertEqual(envelope.event, .unknown(type: "SomeFutureEvent"))
    }

    func testDecodeSceneSnapshotMessage() throws {
        let json = """
        {"kind":"scene_snapshot","snapshot":{
          "scenes":[{"scene_id":"018f5ca9-e8f4-7fd3-a923-4b7a25a6f4d1","name":"shot",
            "objects":[{"object_id":"018f5ca9-e8f4-7fd3-a923-4b7a25a6f4d2","name":"camera",
              "attributes":[{"name":"position",
                "default_value":{"Vec3f":[0.0,0.0,0.0]},
                "current_value":{"Vec3f":[1.0,2.0,3.0]},
                "live_enabled":true,"record_enabled":true}]}]}],
          "mappings":[],
          "mode":{"data_flow":"Idle","recording":"Inactive"},
          "active_scene":null,
          "sessions":[{"session_id":"018f5ca9-e8f4-7fd3-a923-4b7a25a6f4d3",
            "device_id":"018f5ca9-e8f4-7fd3-a923-4b7a25a6f4d4",
            "device_name":"host","roles":["SceneAuthor","Operator"],"is_host":true}],
          "takes":[{"take_id":"018f5ca9-e8f4-7fd3-a923-4b7a25a6f4d5",
            "scene_id":"018f5ca9-e8f4-7fd3-a923-4b7a25a6f4d1",
            "name":"Take 001","path":"/tmp/take.cmtrk","created_ns":1,
            "frame_count":42,"selected":true,"deleted":false}],
          "playback":null,
          "seq":17}}
        """
        let update = try XCTUnwrap(StateEventUpdate(streamJSON: json))
        guard case .snapshot(let snapshot) = update else {
            return XCTFail("expected .snapshot, got \(update)")
        }
        XCTAssertEqual(snapshot.seq, 17)
        XCTAssertEqual(snapshot.scenes.count, 1)
        XCTAssertEqual(snapshot.scenes[0].objects[0].attributes[0].currentValue, .vec3f([1, 2, 3]))
        XCTAssertNil(snapshot.activeScene)
        XCTAssertEqual(snapshot.mode, .idle)
        XCTAssertEqual(snapshot.sessions[0].roles, [.sceneAuthor, .operator])
        XCTAssertTrue(snapshot.sessions[0].isHost)
        XCTAssertEqual(snapshot.takes[0].name, "Take 001")
        XCTAssertEqual(snapshot.takes[0].frameCount, 42)
    }

    func testRejectCodeWireNamesRoundTrip() {
        for code: RejectCode in [
            .unsupportedProtocol, .versionMismatch, .noCommonFeature,
            .authFailed, .roleDenied, .capacityExceeded, .serverBusy,
        ] {
            XCTAssertEqual(RejectCode(wireName: code.wireName), code)
        }
        XCTAssertEqual(RejectCode(wireName: "Whatever"), .unknown("Whatever"))
    }
}
