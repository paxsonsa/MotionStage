import Foundation
import MotionStageSwiftFFI

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
}

public final class MotionStageClient {
    private let rawClient: UnsafeMutableRawPointer

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

    deinit {
        _ = motionstage_swift_client_disconnect(rawClient)
        motionstage_swift_client_free(rawClient)
    }

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

    public func sendPosition(x: Float, y: Float, z: Float) throws {
        let status = motionstage_swift_client_send_vec3f(rawClient, x, y, z)
        try checkStatus(status)
    }

    public func sendPosition(_ value: SIMD3<Float>) throws {
        try sendPosition(x: value.x, y: value.y, z: value.z)
    }

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

    public var sessionID: String? {
        takeRustString(motionstage_swift_client_session_id(rawClient))
    }

    public var deviceID: String? {
        takeRustString(motionstage_swift_client_device_id(rawClient))
    }

    public var lastErrorMessage: String? {
        takeRustString(motionstage_swift_client_last_error(rawClient))
    }

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
