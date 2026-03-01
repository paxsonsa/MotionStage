import Foundation
import MotionStageSwiftFFI
import simd

public struct MotionStageError: Error, CustomStringConvertible {
    public let statusCode: Int32
    public let message: String

    public var description: String {
        "MotionStage error (status=\(statusCode)): \(message)"
    }
}

public enum RuntimeMode: Int32 {
    case idle = 0
    case live = 1
    case recording = 2
    case playback = 3
}

public struct FieldMask: OptionSet, Sendable {
    public let rawValue: UInt32

    public init(rawValue: UInt32) {
        self.rawValue = rawValue
    }

    public static let position      = FieldMask(rawValue: UInt32(MOTIONSTAGE_SWIFT_FIELD_POSITION))
    public static let rotation      = FieldMask(rawValue: UInt32(MOTIONSTAGE_SWIFT_FIELD_ROTATION))
    public static let velocity      = FieldMask(rawValue: UInt32(MOTIONSTAGE_SWIFT_FIELD_VELOCITY))
    public static let focalLength   = FieldMask(rawValue: UInt32(MOTIONSTAGE_SWIFT_FIELD_FOCAL_LENGTH))
    public static let focusDistance  = FieldMask(rawValue: UInt32(MOTIONSTAGE_SWIFT_FIELD_FOCUS_DISTANCE))
    public static let aperture      = FieldMask(rawValue: UInt32(MOTIONSTAGE_SWIFT_FIELD_APERTURE))

    public static let allMotion: FieldMask = [.position, .rotation, .velocity]
    public static let allCamera: FieldMask = [.focalLength, .focusDistance, .aperture]
    public static let all: FieldMask = [.allMotion, .allCamera]
}

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

public final class MotionStageClient: @unchecked Sendable {
    private let rawClient: UnsafeMutableRawPointer

    // MARK: - Legacy single-attribute init

    public init(deviceName: String, outputAttribute: String = "camera.position") throws {
        guard !deviceName.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            throw MotionStageError(statusCode: MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT, message: "deviceName must not be empty")
        }
        guard !outputAttribute.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            throw MotionStageError(statusCode: MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT, message: "outputAttribute must not be empty")
        }

        let maybeClient = deviceName.withCString { deviceNamePtr in
            outputAttribute.withCString { outputAttributePtr in
                motionstage_swift_client_new(deviceNamePtr, outputAttributePtr)
            }
        }

        guard let rawClient = maybeClient else {
            throw MotionStageError(statusCode: MOTIONSTAGE_SWIFT_STATUS_INTERNAL, message: "failed to allocate MotionStage client")
        }

        self.rawClient = rawClient
    }

    // MARK: - Multi-attribute init

    public init(deviceName: String, outputAttributes: [String]) throws {
        guard !deviceName.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            throw MotionStageError(statusCode: MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT, message: "deviceName must not be empty")
        }
        guard !outputAttributes.isEmpty else {
            throw MotionStageError(statusCode: MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT, message: "outputAttributes must not be empty")
        }

        let csv = outputAttributes.joined(separator: ",")

        let maybeClient = deviceName.withCString { deviceNamePtr in
            csv.withCString { csvPtr in
                motionstage_swift_client_new_multi(deviceNamePtr, csvPtr)
            }
        }

        guard let rawClient = maybeClient else {
            throw MotionStageError(statusCode: MOTIONSTAGE_SWIFT_STATUS_INTERNAL, message: "failed to allocate MotionStage client")
        }

        self.rawClient = rawClient
    }

    deinit {
        _ = motionstage_swift_client_disconnect(rawClient)
        motionstage_swift_client_free(rawClient)
    }

    // MARK: - Connection

    public func connect(serverAddress: String, pairingToken: String? = nil, apiKey: String? = nil) throws {
        try serverAddress.withCString { serverAddrPtr in
            try withOptionalCString(pairingToken) { pairingTokenPtr in
                try withOptionalCString(apiKey) { apiKeyPtr in
                    let status = motionstage_swift_client_connect(
                        rawClient,
                        serverAddrPtr,
                        pairingTokenPtr,
                        apiKeyPtr
                    )
                    try checkStatus(status)
                }
            }
        }
    }

    public func disconnect() {
        _ = motionstage_swift_client_disconnect(rawClient)
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

    public func resetScene() throws {
        let status = motionstage_swift_client_reset_scene(rawClient)
        try checkStatus(status)
    }

    // MARK: - Mode

    @discardableResult
    public func setMode(_ mode: RuntimeMode) throws -> RuntimeMode {
        var activeModeRaw: Int32 = MOTIONSTAGE_SWIFT_MODE_IDLE
        let status = motionstage_swift_client_set_mode(rawClient, mode.rawValue, &activeModeRaw)
        try checkStatus(status)

        guard let activeMode = RuntimeMode(rawValue: activeModeRaw) else {
            throw MotionStageError(
                statusCode: MOTIONSTAGE_SWIFT_STATUS_PROTOCOL,
                message: "received unsupported mode value: \(activeModeRaw)"
            )
        }

        return activeMode
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

    // MARK: - Private

    private func checkStatus(_ status: Int32) throws {
        guard status == MOTIONSTAGE_SWIFT_STATUS_OK else {
            throw MotionStageError(
                statusCode: status,
                message: lastErrorMessage ?? "operation failed with status \(status)"
            )
        }
    }
}

private func takeRustString(_ pointer: UnsafeMutablePointer<CChar>?) -> String? {
    guard let pointer else {
        return nil
    }

    defer {
        motionstage_swift_string_free(pointer)
    }

    return String(cString: pointer)
}

private func withOptionalCString<T>(
    _ value: String?,
    body: (UnsafePointer<CChar>?) throws -> T
) rethrows -> T {
    guard let value else {
        return try body(nil)
    }

    return try value.withCString { ptr in
        try body(ptr)
    }
}
