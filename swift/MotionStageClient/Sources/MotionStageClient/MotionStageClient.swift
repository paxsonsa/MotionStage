import Foundation
import MotionStageSwiftFFI
import simd

// MARK: - Error

public enum MotionStageError: Error, LocalizedError, CustomStringConvertible {
    case invalidArgument(String)
    case notConnected
    case alreadyConnected
    case protocolError(String)
    case transportError(String)
    case internalError(String)

    public var description: String {
        switch self {
        case .invalidArgument(let msg): return "MotionStage invalid argument: \(msg)"
        case .notConnected: return "MotionStage error: not connected"
        case .alreadyConnected: return "MotionStage error: already connected"
        case .protocolError(let msg): return "MotionStage protocol error: \(msg)"
        case .transportError(let msg): return "MotionStage transport error: \(msg)"
        case .internalError(let msg): return "MotionStage internal error: \(msg)"
        }
    }

    public var errorDescription: String? { description }
}

// MARK: - RuntimeMode (3.0)

public enum DataFlowState: Int32, Sendable {
    case idle = 0
    case live = 1
}

public enum RecordingState: Int32, Sendable {
    case inactive = 0
    case recording = 1
    case playback = 2
}

public struct RuntimeMode: Sendable, Equatable {
    public let dataFlow: DataFlowState
    public let recording: RecordingState

    public init(dataFlow: DataFlowState, recording: RecordingState) {
        self.dataFlow = dataFlow
        self.recording = recording
    }

    public static let idle = RuntimeMode(dataFlow: .idle, recording: .inactive)
    public static let live = RuntimeMode(dataFlow: .live, recording: .inactive)
    public static let recording = RuntimeMode(dataFlow: .live, recording: .recording)
    public static let playback = RuntimeMode(dataFlow: .live, recording: .playback)
}

// MARK: - FieldMask

public struct FieldMask: OptionSet, Sendable {
    public let rawValue: UInt32

    public init(rawValue: UInt32) {
        self.rawValue = rawValue
    }

    public static let position      = FieldMask(rawValue: UInt32(MOTIONSTAGE_SWIFT_FIELD_POSITION))
    public static let rotation      = FieldMask(rawValue: UInt32(MOTIONSTAGE_SWIFT_FIELD_ROTATION))
    public static let velocity      = FieldMask(rawValue: UInt32(MOTIONSTAGE_SWIFT_FIELD_VELOCITY))
    public static let focalLength   = FieldMask(rawValue: UInt32(MOTIONSTAGE_SWIFT_FIELD_FOCAL_LENGTH))
    public static let focusDistance = FieldMask(rawValue: UInt32(MOTIONSTAGE_SWIFT_FIELD_FOCUS_DISTANCE))
    public static let aperture      = FieldMask(rawValue: UInt32(MOTIONSTAGE_SWIFT_FIELD_APERTURE))

    public static let allMotion: FieldMask = [.position, .rotation, .velocity]
    public static let allCamera: FieldMask = [.focalLength, .focusDistance, .aperture]
    public static let all: FieldMask = [.allMotion, .allCamera]
}

// MARK: - ConnectionEvent (5.3)

public enum ConnectionEvent: Sendable {
    case connected(sessionID: String)
    case disconnected
    case error(MotionStageError)
}

// MARK: - Legacy MotionFrame (deprecated — use CameraMotionFrame)

@available(*, deprecated, renamed: "CameraMotionFrame")
public struct MotionFrame: Sendable {
    public var position: SIMD3<Float>
    public var rotation: simd_quatf
    public var velocity: SIMD3<Float>
    public var focalLength: Float
    public var focusDistance: Float
    public var aperture: Float
    public var fieldMask: FieldMask

    public init(
        position: SIMD3<Float> = .zero,
        rotation: simd_quatf = simd_quatf(ix: 0, iy: 0, iz: 0, r: 1),
        velocity: SIMD3<Float> = .zero,
        focalLength: Float = 50.0,
        focusDistance: Float = 1.0,
        aperture: Float = 2.8,
        fieldMask: FieldMask = .allMotion
    ) {
        self.position = position
        self.rotation = rotation
        self.velocity = velocity
        self.focalLength = focalLength
        self.focusDistance = focusDistance
        self.aperture = aperture
        self.fieldMask = fieldMask
    }
}

// MARK: - ContinuationBox (for async connect bridge)

/// Wraps a `CheckedContinuation` so it can be passed as a `void *` through C callbacks.
private final class ContinuationBox<T>: @unchecked Sendable {
    let cont: CheckedContinuation<T, Error>
    init(_ cont: CheckedContinuation<T, Error>) { self.cont = cont }
}

// MARK: - MotionStageClient

public final class MotionStageClient: @unchecked Sendable {
    private let rawClient: UnsafeMutableRawPointer

    // MARK: Connection events (5.3)

    private let eventContinuation: AsyncStream<ConnectionEvent>.Continuation
    public let events: AsyncStream<ConnectionEvent>

    // MARK: - Init: legacy single-attribute

    public init(deviceName: String, outputAttribute: String = "camera.position") throws {
        guard !deviceName.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            throw MotionStageError.invalidArgument("deviceName must not be empty")
        }
        guard !outputAttribute.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            throw MotionStageError.invalidArgument("outputAttribute must not be empty")
        }

        let maybeClient = deviceName.withCString { deviceNamePtr in
            outputAttribute.withCString { outputAttributePtr in
                motionstage_swift_client_new(deviceNamePtr, outputAttributePtr)
            }
        }

        guard let raw = maybeClient else {
            throw MotionStageError.internalError("failed to allocate MotionStage client")
        }

        self.rawClient = raw
        (self.events, self.eventContinuation) = AsyncStream<ConnectionEvent>.makeStream(
            bufferingPolicy: .bufferingNewest(16)
        )
    }

    // MARK: - Init: typed [AttributeKey] (5.1)

    public init(deviceName: String, outputAttributes: [AttributeKey]) throws {
        guard !deviceName.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            throw MotionStageError.invalidArgument("deviceName must not be empty")
        }
        guard !outputAttributes.isEmpty else {
            throw MotionStageError.invalidArgument("outputAttributes must not be empty")
        }

        let raw = try MotionStageClient.makeClientV3(deviceName: deviceName, attributes: outputAttributes)
        self.rawClient = raw
        (self.events, self.eventContinuation) = AsyncStream<ConnectionEvent>.makeStream(
            bufferingPolicy: .bufferingNewest(16)
        )
    }

    // MARK: - Init: string-keyed multi-attribute (deprecated — use [AttributeKey])

    @available(*, deprecated, message: "Use init(deviceName:outputAttributes:[AttributeKey]) with typed AttributeKey values")
    public init(deviceName: String, outputAttributes: [String]) throws {
        guard !deviceName.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            throw MotionStageError.invalidArgument("deviceName must not be empty")
        }
        guard !outputAttributes.isEmpty else {
            throw MotionStageError.invalidArgument("outputAttributes must not be empty")
        }

        let raw = try MotionStageClient.makeClientV2(deviceName: deviceName, paths: outputAttributes)
        self.rawClient = raw
        (self.events, self.eventContinuation) = AsyncStream<ConnectionEvent>.makeStream(
            bufferingPolicy: .bufferingNewest(16)
        )
    }

    deinit {
        _ = motionstage_swift_client_disconnect(rawClient)
        motionstage_swift_client_free(rawClient)
        eventContinuation.finish()
    }

    // MARK: - Connection

    /// Async connect (2.4). Awaitable from Swift actor contexts without `Task.detached`.
    public func connect(
        serverAddress: String,
        pairingToken: String? = nil,
        apiKey: String? = nil
    ) async throws {
        try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Void, Error>) in
            let box = Unmanaged.passRetained(ContinuationBox(cont))
            let ctx = box.toOpaque()

            serverAddress.withCString { serverAddrPtr in
                withOptionalCString(pairingToken) { pairingTokenPtr in
                    withOptionalCString(apiKey) { apiKeyPtr in
                        motionstage_swift_client_connect_async(
                            rawClient,
                            serverAddrPtr,
                            pairingTokenPtr,
                            apiKeyPtr,
                            connectCallback,
                            ctx
                        )
                    }
                }
            }
        }
        eventContinuation.yield(.connected(sessionID: sessionID ?? "unknown"))
    }

    /// Async connect with certificate pinning (TOFU / 3.1).
    /// `fingerprint` is the 64-char hex SHA-256 of the server's DER certificate,
    /// typically obtained from mDNS discovery.
    public func connect(
        serverAddress: String,
        fingerprint: String,
        pairingToken: String? = nil,
        apiKey: String? = nil
    ) throws {
        try serverAddress.withCString { serverAddrPtr in
            try fingerprint.withCString { fpPtr in
                try withOptionalCString(pairingToken) { pairingTokenPtr in
                    try withOptionalCString(apiKey) { apiKeyPtr in
                        let status = motionstage_swift_client_connect_pinned(
                            rawClient, serverAddrPtr, pairingTokenPtr, apiKeyPtr, fpPtr
                        )
                        try checkStatus(status)
                    }
                }
            }
        }
        eventContinuation.yield(.connected(sessionID: sessionID ?? "unknown"))
    }

    /// Synchronous connect (deprecated — prefer the async overload).
    @available(*, deprecated, message: "Use async connect(serverAddress:pairingToken:apiKey:) instead")
    public func connectSync(
        serverAddress: String,
        pairingToken: String? = nil,
        apiKey: String? = nil
    ) throws {
        try serverAddress.withCString { serverAddrPtr in
            try withOptionalCString(pairingToken) { pairingTokenPtr in
                try withOptionalCString(apiKey) { apiKeyPtr in
                    let status = motionstage_swift_client_connect(
                        rawClient, serverAddrPtr, pairingTokenPtr, apiKeyPtr
                    )
                    try checkStatus(status)
                }
            }
        }
    }

    public func disconnect() {
        _ = motionstage_swift_client_disconnect(rawClient)
        eventContinuation.yield(.disconnected)
    }

    // MARK: - General batch send (2.1)

    /// Send multiple attribute updates in a single datagram.
    public func sendBatch(_ updates: [(key: AttributeKey, values: [Float])]) throws {
        guard !updates.isEmpty else { return }

        // Heap-allocate path C strings so they're stable for the entire call.
        let cPaths = updates.map { $0.key.path.withCString { strdup($0) } }
        defer { cPaths.forEach { free($0) } }

        var floatStorage = updates.map(\.values)   // [[Float]] keeps storage alive
        var cUpdates = [MotionAttributeUpdateC]()
        cUpdates.reserveCapacity(updates.count)

        // Recursive closure to hold UnsafeBufferPointers for all float arrays simultaneously.
        func build(index: Int) throws {
            if index == updates.count {
                let status = cUpdates.withUnsafeBufferPointer {
                    motionstage_swift_client_send_batch(rawClient, $0.baseAddress, UInt32($0.count))
                }
                try checkStatus(status)
                return
            }
            try floatStorage[index].withUnsafeBufferPointer { floatBuf in
                cUpdates.append(MotionAttributeUpdateC(
                    attribute: cPaths[index],
                    data: floatBuf.baseAddress,
                    component_count: UInt32(floatBuf.count)
                ))
                try build(index: index + 1)
            }
        }
        try build(index: 0)
    }

    // MARK: - Legacy motion data

    public func sendPosition(x: Float, y: Float, z: Float) throws {
        let status = motionstage_swift_client_send_vec3f(rawClient, x, y, z)
        try checkStatus(status)
    }

    public func sendPosition(_ value: SIMD3<Float>) throws {
        try sendPosition(x: value.x, y: value.y, z: value.z)
    }

    // MARK: - Multi-attribute motion data

    /// Deprecated: use `CameraMotionFrame.send(via:)` instead.
    @available(*, deprecated, renamed: "CameraMotionFrame.send(via:)")
    public func sendMotionFrame(_ frame: MotionFrame) throws {
        var ffi = MotionFrameFFI(
            position: (frame.position.x, frame.position.y, frame.position.z),
            rotation: (frame.rotation.imag.x, frame.rotation.imag.y, frame.rotation.imag.z, frame.rotation.real),
            velocity: (frame.velocity.x, frame.velocity.y, frame.velocity.z),
            focal_length: frame.focalLength,
            focus_distance: frame.focusDistance,
            aperture: frame.aperture,
            field_mask: frame.fieldMask.rawValue
        )
        let status = motionstage_swift_client_send_motion_frame(rawClient, &ffi)
        try checkStatus(status)
    }

    public func sendVec3(attribute: String, value: SIMD3<Float>) throws {
        try attribute.withCString { attrPtr in
            let status = motionstage_swift_client_send_named_vec3f(rawClient, attrPtr, value.x, value.y, value.z)
            try checkStatus(status)
        }
    }

    public func sendQuaternion(attribute: String, value: simd_quatf) throws {
        try attribute.withCString { attrPtr in
            let status = motionstage_swift_client_send_named_quatf(
                rawClient, attrPtr,
                value.imag.x, value.imag.y, value.imag.z, value.real
            )
            try checkStatus(status)
        }
    }

    public func sendFloat(attribute: String, value: Float) throws {
        try attribute.withCString { attrPtr in
            let status = motionstage_swift_client_send_named_float32(rawClient, attrPtr, value)
            try checkStatus(status)
        }
    }

    // MARK: - Scene control

    /// Async reset. Blocks a thread internally (acceptable for infrequent control ops).
    public func resetScene() async throws {
        let rawClient = self.rawClient
        try await Task.detached(priority: .userInitiated) {
            let status = motionstage_swift_client_reset_scene(rawClient)
            if status != MOTIONSTAGE_SWIFT_STATUS_OK {
                let msg = takeRustString(motionstage_swift_client_last_error(rawClient))
                    ?? "reset scene failed with status \(status)"
                throw makeError(status: status, message: msg)
            }
        }.value
    }

    // MARK: - Mode (3.0)

    /// Set the data flow state. Returns the resulting composite mode.
    @discardableResult
    public func setDataFlow(_ state: DataFlowState) async throws -> RuntimeMode {
        let rawClient = self.rawClient
        return try await Task.detached(priority: .userInitiated) {
            var outDataFlow: Int32 = 0
            var outRecording: Int32 = 0
            let status = motionstage_swift_client_set_data_flow(
                rawClient, state.rawValue, &outDataFlow, &outRecording
            )
            if status != MOTIONSTAGE_SWIFT_STATUS_OK {
                let msg = takeRustString(motionstage_swift_client_last_error(rawClient))
                    ?? "set data flow failed with status \(status)"
                throw makeError(status: status, message: msg)
            }
            return buildRuntimeMode(dataFlow: outDataFlow, recording: outRecording)
        }.value
    }

    /// Set the recording state. Returns the resulting composite mode.
    @discardableResult
    public func setRecording(_ state: RecordingState) async throws -> RuntimeMode {
        let rawClient = self.rawClient
        return try await Task.detached(priority: .userInitiated) {
            var outDataFlow: Int32 = 0
            var outRecording: Int32 = 0
            let status = motionstage_swift_client_set_recording(
                rawClient, state.rawValue, &outDataFlow, &outRecording
            )
            if status != MOTIONSTAGE_SWIFT_STATUS_OK {
                let msg = takeRustString(motionstage_swift_client_last_error(rawClient))
                    ?? "set recording failed with status \(status)"
                throw makeError(status: status, message: msg)
            }
            return buildRuntimeMode(dataFlow: outDataFlow, recording: outRecording)
        }.value
    }

    /// Deprecated convenience: set both axes via composite mode.
    @available(*, deprecated, message: "Use setDataFlow(_:) and setRecording(_:)")
    @discardableResult
    public func setMode(_ mode: RuntimeMode) async throws -> RuntimeMode {
        let rawClient = self.rawClient
        return try await Task.detached(priority: .userInitiated) {
            var activeModeRaw: Int32 = MOTIONSTAGE_SWIFT_MODE_IDLE
            let status = motionstage_swift_client_set_mode(rawClient, mode.legacyRawValue, &activeModeRaw)
            if status != MOTIONSTAGE_SWIFT_STATUS_OK {
                let msg = takeRustString(motionstage_swift_client_last_error(rawClient))
                    ?? "set mode failed with status \(status)"
                throw makeError(status: status, message: msg)
            }
            return RuntimeMode.fromLegacyRaw(activeModeRaw)
        }.value
    }

    // MARK: - Accessors

    public var sessionID: String? {
        takeRustString(motionstage_swift_client_session_id(rawClient))
    }

    public var deviceID: String? {
        takeRustString(motionstage_swift_client_device_id(rawClient))
    }

    public var lastErrorMessage: String? {
        takeRustString(motionstage_swift_client_last_error(rawClient))
    }

    // MARK: - Private helpers

    private func checkStatus(_ status: Int32) throws {
        guard status != MOTIONSTAGE_SWIFT_STATUS_OK else { return }
        let msg = lastErrorMessage ?? "operation failed with status \(status)"
        throw makeError(status: status, message: msg)
    }

    /// Creates a client via the array-based _new_v2 constructor.
    private static func makeClientV2(
        deviceName: String,
        paths: [String]
    ) throws -> UnsafeMutableRawPointer {
        // Heap-allocate C strings for stable pointers during the FFI call.
        let cPaths = paths.map { $0.withCString { strdup($0) } }
        defer { cPaths.forEach { free($0) } }

        var ptrs: [UnsafePointer<CChar>?] = cPaths.map { UnsafePointer($0) }
        let maybeClient = ptrs.withUnsafeMutableBufferPointer { ptrBuf in
            deviceName.withCString { namePtr in
                motionstage_swift_client_new_v2(namePtr, UInt32(paths.count), ptrBuf.baseAddress)
            }
        }

        guard let raw = maybeClient else {
            throw MotionStageError.internalError("failed to allocate MotionStage client")
        }
        return raw
    }

    /// Creates a client via the typed _new_v3 constructor.
    private static func makeClientV3(
        deviceName: String,
        attributes: [AttributeKey]
    ) throws -> UnsafeMutableRawPointer {
        let cPaths = attributes.map { $0.path.withCString { strdup($0) } }
        defer { cPaths.forEach { free($0) } }

        var descriptors = attributes.enumerated().map { (i, attr) in
            MotionStageAttributeDescriptorC(
                path: cPaths[i],
                value_type: attr.kind.cOrdinal
            )
        }

        let maybeClient = descriptors.withUnsafeMutableBufferPointer { descBuf in
            deviceName.withCString { namePtr in
                motionstage_swift_client_new_v3(
                    namePtr,
                    UInt32(attributes.count),
                    descBuf.baseAddress
                )
            }
        }

        guard let raw = maybeClient else {
            throw MotionStageError.internalError("failed to allocate MotionStage client")
        }
        return raw
    }
}

// MARK: - Module-level C callback (must be @convention(c) — no captures)

/// Top-level callback for `motionstage_swift_client_connect_async`.
/// Uses `ctx` as a retained `ContinuationBox<Void>`.
private let connectCallback: MotionStageConnectCallback = { status, errorPtr, ctx in
    let box = Unmanaged<ContinuationBox<Void>>
        .fromOpaque(ctx!)
        .takeRetainedValue()
    if status == MOTIONSTAGE_SWIFT_STATUS_OK {
        box.cont.resume()
    } else {
        let msg = errorPtr.map { String(cString: $0) }
            ?? "connect failed with status \(status)"
        box.cont.resume(throwing: makeError(status: status, message: msg))
    }
}

// MARK: - Free helpers

private func makeError(status: Int32, message: String) -> MotionStageError {
    switch status {
    case MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT: return .invalidArgument(message)
    case MOTIONSTAGE_SWIFT_STATUS_NOT_CONNECTED:    return .notConnected
    case MOTIONSTAGE_SWIFT_STATUS_ALREADY_CONNECTED: return .alreadyConnected
    case MOTIONSTAGE_SWIFT_STATUS_PROTOCOL:         return .protocolError(message)
    case MOTIONSTAGE_SWIFT_STATUS_TRANSPORT:        return .transportError(message)
    default:                                         return .internalError(message)
    }
}

private func takeRustString(_ pointer: UnsafeMutablePointer<CChar>?) -> String? {
    guard let pointer else { return nil }
    defer { motionstage_swift_string_free(pointer) }
    return String(cString: pointer)
}

private func withOptionalCString<T>(
    _ value: String?,
    body: (UnsafePointer<CChar>?) throws -> T
) rethrows -> T {
    guard let value else { return try body(nil) }
    return try value.withCString { try body($0) }
}

private func buildRuntimeMode(dataFlow: Int32, recording: Int32) -> RuntimeMode {
    RuntimeMode(
        dataFlow: DataFlowState(rawValue: dataFlow) ?? .idle,
        recording: RecordingState(rawValue: recording) ?? .inactive
    )
}

extension RuntimeMode {
    /// Map composite mode to legacy integer for deprecated set_mode FFI.
    var legacyRawValue: Int32 {
        switch (dataFlow, recording) {
        case (.idle, _):         return MOTIONSTAGE_SWIFT_MODE_IDLE
        case (.live, .inactive): return MOTIONSTAGE_SWIFT_MODE_LIVE
        case (.live, .recording): return MOTIONSTAGE_SWIFT_MODE_RECORDING
        case (.live, .playback): return MOTIONSTAGE_SWIFT_MODE_PLAYBACK
        }
    }

    /// Reconstruct from legacy integer.
    static func fromLegacyRaw(_ raw: Int32) -> RuntimeMode {
        switch raw {
        case MOTIONSTAGE_SWIFT_MODE_LIVE:      return .live
        case MOTIONSTAGE_SWIFT_MODE_RECORDING: return .recording
        case MOTIONSTAGE_SWIFT_MODE_PLAYBACK:  return .playback
        default:                                return .idle
        }
    }
}
