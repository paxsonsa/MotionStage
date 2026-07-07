import Foundation

// MARK: - Wire enums (operator plane, protocol 2.1)

/// Typed rejection code carried by `MotionStageError.operationRejected`.
/// Mirrors the protocol `RejectCode`; unrecognised wire names decode to
/// `.unknown` so newer servers stay usable.
public enum RejectCode: Sendable, Equatable, Hashable {
    case unsupportedProtocol
    case versionMismatch
    case noCommonFeature
    case authFailed
    case roleDenied
    case capacityExceeded
    case serverBusy
    case unknown(String)

    public init(wireName: String) {
        switch wireName {
        case "UnsupportedProtocol": self = .unsupportedProtocol
        case "VersionMismatch": self = .versionMismatch
        case "NoCommonFeature": self = .noCommonFeature
        case "AuthFailed": self = .authFailed
        case "RoleDenied": self = .roleDenied
        case "CapacityExceeded": self = .capacityExceeded
        case "ServerBusy": self = .serverBusy
        default: self = .unknown(wireName)
        }
    }

    public var wireName: String {
        switch self {
        case .unsupportedProtocol: return "UnsupportedProtocol"
        case .versionMismatch: return "VersionMismatch"
        case .noCommonFeature: return "NoCommonFeature"
        case .authFailed: return "AuthFailed"
        case .roleDenied: return "RoleDenied"
        case .capacityExceeded: return "CapacityExceeded"
        case .serverBusy: return "ServerBusy"
        case .unknown(let name): return name
        }
    }
}

/// Mirrors the protocol `ClientRole`.
public enum ClientRole: Sendable, Equatable, Hashable, Decodable {
    case motionSource
    case cameraController
    case videoSink
    case `operator`
    case sceneAuthor
    case unknown(String)

    public init(wireName: String) {
        switch wireName {
        case "MotionSource": self = .motionSource
        case "CameraController": self = .cameraController
        case "VideoSink": self = .videoSink
        case "Operator": self = .operator
        case "SceneAuthor": self = .sceneAuthor
        default: self = .unknown(wireName)
        }
    }

    public init(from decoder: Decoder) throws {
        self.init(wireName: try decoder.singleValueContainer().decode(String.self))
    }
}

/// Mirrors the protocol `BaselineAction`.
public enum BaselineAction: Sendable, Equatable, Decodable {
    case resetScene
    case commitScene
    case commitObject
    case unknown(String)

    public init(wireName: String) {
        switch wireName {
        case "ResetScene": self = .resetScene
        case "CommitScene": self = .commitScene
        case "CommitObject": self = .commitObject
        default: self = .unknown(wireName)
        }
    }

    public init(from decoder: Decoder) throws {
        self.init(wireName: try decoder.singleValueContainer().decode(String.self))
    }
}

/// Mirrors the protocol `PlaybackRuntimeState`.
public enum PlaybackState: Sendable, Equatable, Decodable {
    case stopped
    case playing
    case paused
    case unknown(String)

    public init(wireName: String) {
        switch wireName {
        case "Stopped": self = .stopped
        case "Playing": self = .playing
        case "Paused": self = .paused
        default: self = .unknown(wireName)
        }
    }

    public init(from decoder: Decoder) throws {
        self.init(wireName: try decoder.singleValueContainer().decode(String.self))
    }
}

// MARK: - RuntimeMode wire decoding

extension DataFlowState {
    init?(wireName: String) {
        switch wireName {
        case "Idle": self = .idle
        case "Live": self = .live
        default: return nil
        }
    }
}

extension RecordingState {
    init?(wireName: String) {
        switch wireName {
        case "Inactive": self = .inactive
        case "Recording": self = .recording
        case "Playback": self = .playback
        default: return nil
        }
    }
}

extension RuntimeMode: Decodable {
    private enum CodingKeys: String, CodingKey {
        case dataFlow = "data_flow"
        case recording
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let dataFlowName = try container.decode(String.self, forKey: .dataFlow)
        let recordingName = try container.decode(String.self, forKey: .recording)
        guard let dataFlow = DataFlowState(wireName: dataFlowName),
              let recording = RecordingState(wireName: recordingName)
        else {
            throw DecodingError.dataCorruptedError(
                forKey: .dataFlow, in: container,
                debugDescription: "unknown mode \(dataFlowName)/\(recordingName)"
            )
        }
        self.init(dataFlow: dataFlow, recording: recording)
    }
}

// MARK: - Attribute values

/// Mirrors the protocol `BakeAttributeValue` (externally tagged on the wire:
/// `{"Vec3f":[1,2,3]}`).
public enum AttributeValue: Sendable, Equatable {
    case bool(Bool)
    case int32(Int32)
    case float32(Float)
    case float64(Double)
    case vec2f([Float])
    case vec3f([Float])
    case vec4f([Float])
    case quatf([Float])
    case mat4f([[Float]])
    case trigger(Bool)
    case unknown(type: String)
}

extension AttributeValue: Decodable {
    private struct VariantKey: CodingKey {
        var stringValue: String
        var intValue: Int? { nil }
        init?(stringValue: String) { self.stringValue = stringValue }
        init?(intValue: Int) { return nil }
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: VariantKey.self)
        guard let key = container.allKeys.first, container.allKeys.count == 1 else {
            throw DecodingError.dataCorrupted(DecodingError.Context(
                codingPath: decoder.codingPath,
                debugDescription: "attribute value must be a single-variant object"
            ))
        }
        switch key.stringValue {
        case "Bool": self = .bool(try container.decode(Bool.self, forKey: key))
        case "Int32": self = .int32(try container.decode(Int32.self, forKey: key))
        case "Float32": self = .float32(try container.decode(Float.self, forKey: key))
        case "Float64": self = .float64(try container.decode(Double.self, forKey: key))
        case "Vec2f": self = .vec2f(try container.decode([Float].self, forKey: key))
        case "Vec3f": self = .vec3f(try container.decode([Float].self, forKey: key))
        case "Vec4f": self = .vec4f(try container.decode([Float].self, forKey: key))
        case "Quatf": self = .quatf(try container.decode([Float].self, forKey: key))
        case "Mat4f": self = .mat4f(try container.decode([[Float]].self, forKey: key))
        case "Trigger": self = .trigger(try container.decode(Bool.self, forKey: key))
        default: self = .unknown(type: key.stringValue)
        }
    }
}

// MARK: - Mapping / take summaries

/// Mirrors the protocol `MappingSummary` (post-default-resolution: the
/// server-resolved source device and target scene are always concrete).
public struct MappingSummary: Sendable, Equatable, Decodable {
    public let mappingID: UUID
    public let sourceDevice: UUID
    public let sourceOutput: String
    public let targetScene: UUID
    public let targetObject: UUID
    public let targetAttribute: String
    public let componentMask: [Int]?
    public let lock: Bool

    public init(
        mappingID: UUID,
        sourceDevice: UUID,
        sourceOutput: String,
        targetScene: UUID,
        targetObject: UUID,
        targetAttribute: String,
        componentMask: [Int]?,
        lock: Bool
    ) {
        self.mappingID = mappingID
        self.sourceDevice = sourceDevice
        self.sourceOutput = sourceOutput
        self.targetScene = targetScene
        self.targetObject = targetObject
        self.targetAttribute = targetAttribute
        self.componentMask = componentMask
        self.lock = lock
    }

    enum CodingKeys: String, CodingKey {
        case mappingID = "mapping_id"
        case sourceDevice = "source_device"
        case sourceOutput = "source_output"
        case targetScene = "target_scene"
        case targetObject = "target_object"
        case targetAttribute = "target_attribute"
        case componentMask = "component_mask"
        case lock
    }
}

/// Mirrors the protocol `TakeInfo` (a registered take-catalog entry; `path`
/// is the server-side recording path).
public struct TakeInfo: Sendable, Equatable, Decodable {
    public let takeID: UUID
    public let sceneID: UUID
    public let name: String
    public let path: String
    public let createdNs: UInt64
    public let frameCount: UInt64
    public let selected: Bool
    public let deleted: Bool

    enum CodingKeys: String, CodingKey {
        case takeID = "take_id"
        case sceneID = "scene_id"
        case name
        case path
        case createdNs = "created_ns"
        case frameCount = "frame_count"
        case selected
        case deleted
    }
}

// MARK: - Scene snapshot

/// Mirrors the protocol `SnapshotAttribute`.
public struct SnapshotAttribute: Sendable, Equatable, Decodable {
    public let name: String
    public let defaultValue: AttributeValue
    public let currentValue: AttributeValue
    public let liveEnabled: Bool
    public let recordEnabled: Bool

    enum CodingKeys: String, CodingKey {
        case name
        case defaultValue = "default_value"
        case currentValue = "current_value"
        case liveEnabled = "live_enabled"
        case recordEnabled = "record_enabled"
    }
}

/// Mirrors the protocol `SnapshotObject`.
public struct SnapshotObject: Sendable, Equatable, Decodable {
    public let objectID: UUID
    public let name: String
    public let attributes: [SnapshotAttribute]

    enum CodingKeys: String, CodingKey {
        case objectID = "object_id"
        case name
        case attributes
    }
}

/// Mirrors the protocol `SnapshotScene`.
public struct SnapshotScene: Sendable, Equatable, Decodable {
    public let sceneID: UUID
    public let name: String
    public let objects: [SnapshotObject]

    enum CodingKeys: String, CodingKey {
        case sceneID = "scene_id"
        case name
        case objects
    }
}

/// Mirrors the protocol `SessionSummary`.
public struct SessionSummary: Sendable, Equatable, Decodable {
    public let sessionID: UUID
    public let deviceID: UUID
    public let deviceName: String
    public let roles: [ClientRole]
    /// True for the server's in-process host session (the DCC itself).
    public let isHost: Bool

    enum CodingKeys: String, CodingKey {
        case sessionID = "session_id"
        case deviceID = "device_id"
        case deviceName = "device_name"
        case roles
        case isHost = "is_host"
    }
}

/// Mirrors the protocol `PlaybackSummary`.
public struct PlaybackSummary: Sendable, Equatable, Decodable {
    public let state: PlaybackState
    public let takeID: UUID
    public let playheadNs: UInt64
    public let looping: Bool

    enum CodingKeys: String, CodingKey {
        case state
        case takeID = "take_id"
        case playheadNs = "playhead_ns"
        case looping
    }
}

/// Full world snapshot (mirrors the protocol `SceneSnapshotPayload`). `seq`
/// is the event sequence number the snapshot is consistent with: state events
/// with `seq` at or below it are already folded in and can be discarded.
public struct SceneSnapshot: Sendable, Equatable, Decodable {
    public let scenes: [SnapshotScene]
    public let mappings: [MappingSummary]
    public let mode: RuntimeMode
    public let activeScene: UUID?
    public let sessions: [SessionSummary]
    public let takes: [TakeInfo]
    public let playback: PlaybackSummary?
    public let seq: UInt64

    enum CodingKeys: String, CodingKey {
        case scenes
        case mappings
        case mode
        case activeScene = "active_scene"
        case sessions
        case takes
        case playback
        case seq
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        scenes = try container.decode([SnapshotScene].self, forKey: .scenes)
        mappings = try container.decode([MappingSummary].self, forKey: .mappings)
        mode = try container.decode(RuntimeMode.self, forKey: .mode)
        activeScene = try container.decodeIfPresent(UUID.self, forKey: .activeScene)
        sessions = try container.decode([SessionSummary].self, forKey: .sessions)
        takes = try container.decode([TakeInfo].self, forKey: .takes)
        playback = try container.decodeIfPresent(PlaybackSummary.self, forKey: .playback)
        seq = try container.decode(UInt64.self, forKey: .seq)
    }
}

// MARK: - State events

/// Typed mirror of the protocol `StateEvent`. Every server mutation emits
/// exactly one event, fanned out to every session — including the session
/// that caused it (filter on `StateEventEnvelope.originSession` to skip your
/// own echoes). Unknown variants decode to `.unknown` for forward compat.
public enum StateEvent: Sendable, Equatable {
    case modeChanged(mode: RuntimeMode)
    case sceneLoaded(sceneID: UUID, name: String)
    case sceneActivated(sceneID: UUID)
    case mappingCreated(mapping: MappingSummary)
    case mappingUpdated(mapping: MappingSummary)
    case mappingRemoved(mappingID: UUID)
    case mappingLockChanged(mappingID: UUID, lock: Bool)
    case mappingReleased(mappingID: UUID, reason: String)
    case baselineApplied(action: BaselineAction, changedAttributes: UInt32)
    case sessionJoined(sessionID: UUID, deviceID: UUID, deviceName: String, roles: [ClientRole])
    case sessionLeft(sessionID: UUID, reason: String?)
    case recordingStarted(takeID: UUID, sceneID: UUID)
    case recordingStopped(takeID: UUID, frameCount: UInt64)
    case takeRegistered(take: TakeInfo)
    case takeSelected(takeID: UUID, sceneID: UUID)
    case takeDeleted(takeID: UUID)
    case playbackChanged(state: PlaybackState, takeID: UUID, playheadNs: UInt64, looping: Bool)
    /// An event variant this SDK version does not know about.
    case unknown(type: String)
}

extension StateEvent: Decodable {
    private enum CodingKeys: String, CodingKey {
        case type
        case data
    }

    /// All possible variant payload fields (each variant uses a subset).
    private struct Fields: Decodable {
        let mode: RuntimeMode?
        let sceneID: UUID?
        let name: String?
        let mapping: MappingSummary?
        let mappingID: UUID?
        let lock: Bool?
        let reason: String?
        let action: BaselineAction?
        let changedAttributes: UInt32?
        let sessionID: UUID?
        let deviceID: UUID?
        let deviceName: String?
        let roles: [ClientRole]?
        let takeID: UUID?
        let frameCount: UInt64?
        let take: TakeInfo?
        let state: PlaybackState?
        let playheadNs: UInt64?
        let looping: Bool?

        enum CodingKeys: String, CodingKey {
            case mode
            case sceneID = "scene_id"
            case name
            case mapping
            case mappingID = "mapping_id"
            case lock
            case reason
            case action
            case changedAttributes = "changed_attributes"
            case sessionID = "session_id"
            case deviceID = "device_id"
            case deviceName = "device_name"
            case roles
            case takeID = "take_id"
            case frameCount = "frame_count"
            case take
            case state
            case playheadNs = "playhead_ns"
            case looping
        }
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let type = try container.decode(String.self, forKey: .type)

        func fields() throws -> Fields {
            try container.decode(Fields.self, forKey: .data)
        }
        func require<T>(_ value: T?, _ field: String) throws -> T {
            guard let value else {
                throw DecodingError.dataCorruptedError(
                    forKey: .data, in: container,
                    debugDescription: "\(type) event is missing field \(field)"
                )
            }
            return value
        }

        switch type {
        case "ModeChanged":
            let f = try fields()
            self = .modeChanged(mode: try require(f.mode, "mode"))
        case "SceneLoaded":
            let f = try fields()
            self = .sceneLoaded(
                sceneID: try require(f.sceneID, "scene_id"),
                name: try require(f.name, "name")
            )
        case "SceneActivated":
            let f = try fields()
            self = .sceneActivated(sceneID: try require(f.sceneID, "scene_id"))
        case "MappingCreated":
            let f = try fields()
            self = .mappingCreated(mapping: try require(f.mapping, "mapping"))
        case "MappingUpdated":
            let f = try fields()
            self = .mappingUpdated(mapping: try require(f.mapping, "mapping"))
        case "MappingRemoved":
            let f = try fields()
            self = .mappingRemoved(mappingID: try require(f.mappingID, "mapping_id"))
        case "MappingLockChanged":
            let f = try fields()
            self = .mappingLockChanged(
                mappingID: try require(f.mappingID, "mapping_id"),
                lock: try require(f.lock, "lock")
            )
        case "MappingReleased":
            let f = try fields()
            self = .mappingReleased(
                mappingID: try require(f.mappingID, "mapping_id"),
                reason: try require(f.reason, "reason")
            )
        case "BaselineApplied":
            let f = try fields()
            self = .baselineApplied(
                action: try require(f.action, "action"),
                changedAttributes: try require(f.changedAttributes, "changed_attributes")
            )
        case "SessionJoined":
            let f = try fields()
            self = .sessionJoined(
                sessionID: try require(f.sessionID, "session_id"),
                deviceID: try require(f.deviceID, "device_id"),
                deviceName: try require(f.deviceName, "device_name"),
                roles: try require(f.roles, "roles")
            )
        case "SessionLeft":
            let f = try fields()
            self = .sessionLeft(
                sessionID: try require(f.sessionID, "session_id"),
                reason: f.reason
            )
        case "RecordingStarted":
            let f = try fields()
            self = .recordingStarted(
                takeID: try require(f.takeID, "take_id"),
                sceneID: try require(f.sceneID, "scene_id")
            )
        case "RecordingStopped":
            let f = try fields()
            self = .recordingStopped(
                takeID: try require(f.takeID, "take_id"),
                frameCount: try require(f.frameCount, "frame_count")
            )
        case "TakeRegistered":
            let f = try fields()
            self = .takeRegistered(take: try require(f.take, "take"))
        case "TakeSelected":
            let f = try fields()
            self = .takeSelected(
                takeID: try require(f.takeID, "take_id"),
                sceneID: try require(f.sceneID, "scene_id")
            )
        case "TakeDeleted":
            let f = try fields()
            self = .takeDeleted(takeID: try require(f.takeID, "take_id"))
        case "PlaybackChanged":
            let f = try fields()
            self = .playbackChanged(
                state: try require(f.state, "state"),
                takeID: try require(f.takeID, "take_id"),
                playheadNs: try require(f.playheadNs, "playhead_ns"),
                looping: try require(f.looping, "looping")
            )
        default:
            self = .unknown(type: type)
        }
    }
}

/// Ordered wrapper for state-event fan-out (mirrors the protocol
/// `StateEventEnvelope`). `seq` is strictly monotonic in mutation order.
/// `originSession` is the session that caused the mutation (`nil` for
/// server-internal transitions); compare it to `MotionStageClient.sessionID`
/// to recognise your own echoes.
public struct StateEventEnvelope: Sendable, Equatable, Decodable {
    public let seq: UInt64
    public let originSession: UUID?
    public let timestampNs: UInt64
    public let event: StateEvent

    enum CodingKeys: String, CodingKey {
        case seq
        case originSession = "origin_session"
        case timestampNs = "timestamp_ns"
        case event
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        seq = try container.decode(UInt64.self, forKey: .seq)
        originSession = try container.decodeIfPresent(UUID.self, forKey: .originSession)
        timestampNs = try container.decode(UInt64.self, forKey: .timestampNs)
        event = try container.decode(StateEvent.self, forKey: .event)
    }
}

/// One element of `MotionStageClient.stateEvents`.
public enum StateEventUpdate: Sendable, Equatable {
    /// A replicated state change.
    case event(StateEventEnvelope)
    /// An unsolicited full-world snapshot (handshake, resync, lag recovery).
    case snapshot(SceneSnapshot)
}

extension StateEventUpdate: Decodable {
    private enum CodingKeys: String, CodingKey {
        case kind
        case snapshot
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let kind = try container.decode(String.self, forKey: .kind)
        switch kind {
        case "state_event":
            self = .event(try StateEventEnvelope(from: decoder))
        case "scene_snapshot":
            self = .snapshot(try container.decode(SceneSnapshot.self, forKey: .snapshot))
        default:
            throw DecodingError.dataCorruptedError(
                forKey: .kind, in: container,
                debugDescription: "unknown stream message kind \(kind)"
            )
        }
    }

    /// Decode one JSON document from the FFI state-event stream; returns nil
    /// on malformed input.
    init?(streamJSON: String) {
        guard let update = try? JSONDecoder().decode(
            StateEventUpdate.self, from: Data(streamJSON.utf8)
        ) else { return nil }
        self = update
    }
}
